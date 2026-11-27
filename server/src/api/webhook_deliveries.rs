//! Webhook delivery API endpoints.
//!
//! GET  /invoices/{invoice_id}/webhook-deliveries — recent deliveries for an invoice.
//! GET  /stores/{store_id}/webhook-deliveries — recent deliveries for a store.
//! POST /stores/{store_id}/webhook-deliveries/{delivery_id}/replay — resend one.
//!
//! `webhook_deliveries` used to be written by nobody: the retry queue existed,
//! backed off, and threw its history away the moment a job gave up. This is
//! the read side of making that history visible - gated behind
//! `ethpay.store.canmanagewebhooks` (see `require_webhook_permission`), 404
//! for anyone without it on the store named in the path.

#[cfg(test)]
mod tests;

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde::Serialize;
use uuid::Uuid;

use auth::{SessionService, UserStoreRepository};
use data_service::{InvoiceReader, StoreWebhookReader, WebhookDeliveryData, WebhookDeliveryReader};
use types::{InvoiceId, StoreId};

use super::extractors::{StoreScopedUser, key_grants_store_permission};
use crate::services::webhook::{WebhookJob, WebhookPayload};
use crate::state::PgAppState;

/// Gate a webhook-delivery read or replay behind a store permission.
///
/// All three handlers used to check only bare store membership through
/// `AuthenticatedUser`, which carries no API-key scope at all. Reuses
/// `ethpay.store.canmanagewebhooks` - the permission already defined for
/// webhook configuration (Owner and Manager, not Employee or Guest) - for
/// every operation here, reads included: delivery history and replay are
/// both webhook-operations concerns, not merchant-facing business data the
/// way invoices/payments are, so this does not reuse `canviewpayments`.
async fn require_webhook_permission<A: SessionService>(
    state: &PgAppState<A>,
    user: &auth::UserInfo,
    key_scope: Option<&[String]>,
    store_id: StoreId,
) -> Result<(), StatusCode> {
    if user.role.is_admin() {
        return Ok(());
    }
    const MANAGE_WEBHOOKS: &str = "ethpay.store.canmanagewebhooks";
    let has_permission = state
        .data_service
        .user_has_store_permission(user.id, store_id, MANAGE_WEBHOOKS)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        && key_grants_store_permission(key_scope, MANAGE_WEBHOOKS, store_id);

    if has_permission {
        Ok(())
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}

/// Pages are not exposed to the caller yet - the same fixed-size-page
/// starting point `payouts::list_payouts` used before pagination existed.
const DEFAULT_LIMIT: i64 = 50;

/// A delivery attempt as shown to a merchant.
///
/// `last_error` is whatever text the merchant's own endpoint, or the HTTP
/// client trying to reach it, produced - attacker-influenced from the
/// subscriber's point of view. It travels here as a plain JSON string field;
/// nothing in this crate renders it as HTML, and nothing downstream should.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct WebhookDeliveryResponse {
    pub id: Uuid,
    pub invoice_id: String,
    pub event_type: String,
    pub status: String,
    pub attempts: i32,
    pub max_attempts: i32,
    pub last_error: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

impl From<WebhookDeliveryData> for WebhookDeliveryResponse {
    fn from(d: WebhookDeliveryData) -> Self {
        Self {
            id: d.id,
            invoice_id: d.invoice_id,
            event_type: d.event_type,
            status: d.status.to_string(),
            attempts: d.attempts,
            max_attempts: d.max_attempts,
            last_error: d.last_error,
            created_at: d.created_at,
            updated_at: d.updated_at,
        }
    }
}

#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct WebhookDeliveryListResponse {
    pub total: i64,
    pub deliveries: Vec<WebhookDeliveryResponse>,
}

/// Turn a stored payload into a fresh delivery of the same logical event.
///
/// Only `event_id` changes: it identifies one queued delivery (see the
/// `webhook` module docs), so a re-emission gets a new one. `idempotency_key`
/// is left untouched - it is what a subscriber dedupes on, so a replay that
/// changed it would look like a distinct event rather than a resend of the
/// one that was missed.
fn replayed_payload(mut original: WebhookPayload) -> WebhookPayload {
    original.event_id = Uuid::new_v4();
    original
}

/// Load a delivery, refusing one that belongs to a different store.
///
/// Mismatch answers 404, the same as a delivery id that does not exist at
/// all - matching `payouts::payout_for_store` and `refunds`, for the same
/// reason: the difference between "not yours" and "no such row" is itself a
/// fact about another merchant's data.
async fn delivery_for_store<R>(
    reader: &R,
    store_id: Uuid,
    delivery_id: Uuid,
) -> Result<WebhookDeliveryData, StatusCode>
where
    R: WebhookDeliveryReader,
{
    let delivery = WebhookDeliveryReader::get_delivery(reader, delivery_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    if delivery.store_id != store_id {
        tracing::warn!(
            store_id = %store_id,
            "Webhook delivery lookup refused: the delivery belongs to another store"
        );
        return Err(StatusCode::NOT_FOUND);
    }

    Ok(delivery)
}

/// List recent webhook deliveries for an invoice.
#[utoipa::path(
    get,
    path = "/invoices/{invoice_id}/webhook-deliveries",
    tag = "webhook-deliveries",
    security(("bearer_auth" = [])),
    params(
        ("invoice_id" = String, Path, description = "Invoice ID")
    ),
    responses(
        (status = 200, description = "Recent deliveries for this invoice", body = WebhookDeliveryListResponse),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Insufficient permissions"),
        (status = 404, description = "Invoice not found"),
    )
)]
pub async fn list_deliveries_for_invoice<A>(
    StoreScopedUser(user, key_scope): StoreScopedUser,
    State(state): State<PgAppState<A>>,
    Path(invoice_id): Path<String>,
) -> Result<Json<WebhookDeliveryListResponse>, StatusCode>
where
    A: SessionService + 'static,
{
    let id = InvoiceId::from_string(invoice_id);

    let invoice = InvoiceReader::get(&*state.data_service, &id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    require_webhook_permission(&state, &user, key_scope.as_deref(), invoice.store_id).await?;

    let (total, deliveries) = WebhookDeliveryReader::list_deliveries_for_invoice(
        &*state.data_service,
        id.as_str(),
        DEFAULT_LIMIT,
        0,
    )
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(WebhookDeliveryListResponse {
        total,
        deliveries: deliveries.into_iter().map(Into::into).collect(),
    }))
}

/// List recent webhook deliveries for a store.
#[utoipa::path(
    get,
    path = "/stores/{store_id}/webhook-deliveries",
    tag = "webhook-deliveries",
    security(("bearer_auth" = [])),
    params(
        ("store_id" = Uuid, Path, description = "Store ID")
    ),
    responses(
        (status = 200, description = "Recent deliveries for this store", body = WebhookDeliveryListResponse),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Insufficient permissions"),
    )
)]
pub async fn list_deliveries_for_store<A>(
    StoreScopedUser(user, key_scope): StoreScopedUser,
    State(state): State<PgAppState<A>>,
    Path(store_id): Path<Uuid>,
) -> Result<Json<WebhookDeliveryListResponse>, StatusCode>
where
    A: SessionService + 'static,
{
    require_webhook_permission(&state, &user, key_scope.as_deref(), StoreId(store_id)).await?;

    let (total, deliveries) = WebhookDeliveryReader::list_deliveries_for_store(
        &*state.data_service,
        store_id,
        DEFAULT_LIMIT,
        0,
    )
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(WebhookDeliveryListResponse {
        total,
        deliveries: deliveries.into_iter().map(Into::into).collect(),
    }))
}

/// Replay a past delivery.
///
/// The stored payload is the exact one that was queued the first time, not a
/// snapshot of the invoice's current state, so it is sent verbatim except for
/// a fresh `event_id` - a replay is a new *delivery* of the same *event*.
/// `idempotency_key` is derived from the event's identity and is not touched,
/// so a subscriber that dedupes correctly sees no new event.
#[utoipa::path(
    post,
    path = "/stores/{store_id}/webhook-deliveries/{delivery_id}/replay",
    tag = "webhook-deliveries",
    security(("bearer_auth" = [])),
    params(
        ("store_id" = Uuid, Path, description = "Store ID"),
        ("delivery_id" = Uuid, Path, description = "Delivery ID to replay")
    ),
    responses(
        (status = 200, description = "Replay queued", body = WebhookDeliveryResponse),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Insufficient permissions"),
        (status = 404, description = "Delivery not found"),
    )
)]
pub async fn replay_delivery<A>(
    StoreScopedUser(user, key_scope): StoreScopedUser,
    State(state): State<PgAppState<A>>,
    Path((store_id, delivery_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<WebhookDeliveryResponse>, StatusCode>
where
    A: SessionService + 'static,
{
    require_webhook_permission(&state, &user, key_scope.as_deref(), StoreId(store_id)).await?;

    let delivery = delivery_for_store(&*state.data_service, store_id, delivery_id).await?;

    let original: WebhookPayload = serde_json::from_value(delivery.payload).map_err(|e| {
        tracing::error!(
            delivery_id = %delivery_id,
            error = %e,
            "Stored webhook payload failed to deserialize"
        );
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let payload = replayed_payload(original);

    // The store's *current* webhook, not the one this delivery originally
    // went to: rotating the secret or URL must not strand old deliveries
    // behind a config that no longer exists, and a disabled webhook must not
    // be replayed into.
    let webhook_config = StoreWebhookReader::get_enabled_webhook(&*state.data_service, store_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::CONFLICT)?;

    let Some(sink) = &state.webhook_sink else {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    };

    let job = WebhookJob::new(
        webhook_config.id,
        webhook_config.webhook_url,
        webhook_config.webhook_secret,
        payload,
    );

    let response = WebhookDeliveryResponse {
        id: job.id,
        invoice_id: job.payload.invoice_id.clone(),
        event_type: job.payload.event_type.to_string(),
        status: "pending".to_string(),
        attempts: 0,
        max_attempts: job.max_attempts as i32,
        last_error: None,
        created_at: job.created_at,
        updated_at: job.created_at,
    };

    sink.queue(job).await.map_err(|e| {
        tracing::warn!(
            delivery_id = %delivery_id,
            error = %e,
            "Failed to queue webhook replay"
        );
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    Ok(Json(response))
}
