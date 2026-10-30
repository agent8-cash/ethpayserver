use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde::Deserialize;

use ::types::InvoiceReader;
use auth::{SessionService, repository::UserStoreRepository};
use data_service::PaymentOptionReader;

use super::{InvoiceResponse, TxHashLookupResponse, VIEW_INVOICES, customer_email_of};
use crate::api::api_key_hash::is_hex_bytes;
use crate::api::extractors::{StoreScopedUser, key_grants_store_permission};
use crate::state::PgAppState;

/// Validate a tx hash: must be 0x followed by 64 hex characters.
pub(crate) fn is_valid_tx_hash(hash: &str) -> bool {
    is_hex_bytes(hash, 32)
}

/// Path parameters for tx hash lookup.
#[derive(Debug, Deserialize)]
pub struct TxHashLookupPath {
    pub chain_id: String,
    pub tx_hash: String,
}

/// Parse the chain id out of a path segment.
///
/// A CAIP-2 identifier in a path segment needs no escaping: RFC 3986 allows `:`
/// in `pchar`, so `/…/eip155:1/0xabc…` is a legal URL as written.
fn parse_path_chain_id(raw: &str) -> Result<types::ChainId, (StatusCode, Json<serde_json::Value>)> {
    types::ChainId::parse(raw).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": format!("invalid chain id: {e}") })),
        )
    })
}

/// Lookup an invoice by transaction hash.
///
/// Finds the payment matching the given chain_id and tx_hash, then returns
/// the associated invoice. Returns 404 if no match or if the caller doesn't
/// own the invoice's store (prevents enumeration).
#[utoipa::path(
    get,
    path = "/invoices/by-tx/{chain_id}/{tx_hash}",
    tag = "invoices",
    security(("bearer_auth" = [])),
    params(
        ("chain_id" = String, Path, description = "CAIP-2 chain identifier, e.g. eip155:1"),
        ("tx_hash" = String, Path, description = "Transaction hash (0x-prefixed, 64 hex chars)")
    ),
    responses(
        (status = 200, description = "Invoice and payment found", body = TxHashLookupResponse),
        (status = 400, description = "Malformed tx_hash"),
        (status = 401, description = "Unauthorized"),
        (status = 404, description = "No invoice found for this transaction"),
    )
)]
#[allow(clippy::too_many_lines)] // one lookup chain: payment, invoice, store access, key scope
pub async fn lookup_by_tx_hash<A>(
    StoreScopedUser(user, key_scope): StoreScopedUser,
    State(state): State<PgAppState<A>>,
    Path(path): Path<TxHashLookupPath>,
) -> Result<Json<TxHashLookupResponse>, (StatusCode, Json<serde_json::Value>)>
where
    A: SessionService + 'static,
{
    // Validate tx_hash format
    if !is_valid_tx_hash(&path.tx_hash) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "malformed_tx_hash"})),
        ));
    }

    let chain_id = parse_path_chain_id(&path.chain_id)?;

    // Look up payment by (chain_id, tx_hash)
    let payment = state
        .data_service
        .get_payment_by_tx_hash(&chain_id, &path.tx_hash)
        .await
        .map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "internal_error"})),
            )
        })?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "not_found"})),
            )
        })?;

    // Fetch the linked invoice
    let invoice = InvoiceReader::get(&*state.data_service, &payment.invoice_id)
        .await
        .map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "internal_error"})),
            )
        })?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "not_found"})),
            )
        })?;

    // Permission check — non-admins must own the store (return 404 to hide existence)
    if user.role != auth::Role::ServerAdmin {
        let is_member = state
            .data_service
            .get_user_store(user.id, invoice.store_id)
            .await
            .map_err(|_| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": "internal_error"})),
                )
            })?
            .is_some();

        if !is_member {
            return Err((
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "not_found"})),
            ));
        }
    }

    if !key_grants_store_permission(key_scope.as_deref(), VIEW_INVOICES, invoice.store_id) {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "not_found"})),
        ));
    }

    // Get payment options for the invoice
    let options = PaymentOptionReader::get_for_invoice(&*state.data_service, &invoice.id)
        .await
        .unwrap_or_default();

    let customer_email = customer_email_of(&invoice);
    let response = TxHashLookupResponse {
        invoice: InvoiceResponse {
            id: invoice.id.0,
            store_id: invoice.store_id.0.to_string(),
            store_name: None,
            currency: invoice.currency,
            status: invoice.status,
            amount: invoice.amount,
            amount_received: invoice.amount_received,
            created_at: invoice.created_at,
            expires_at: invoice.expires_at,
            metadata: invoice.metadata,
            customer_email,
            payment_options: options.into_iter().map(Into::into).collect(),
        },
        payment: payment.into(),
    };

    Ok(Json(response))
}
