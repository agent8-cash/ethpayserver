use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use uuid::Uuid;

use ::types::{InvoiceId, InvoiceReader, InvoiceStatus, PaymentReader};
use auth::{SessionService, repository::UserStoreRepository};
use data_service::PaymentOptionReader;

use super::{
    InvoiceStatusResponse, ListPaymentsQuery, PaymentListResponse, PaymentResponse, StoreScope,
    VIEW_INVOICES, build_payment_filter_params, get_invoice_with_permission, narrow_scope_by_key,
    resolve_store_names, verify_store_access_for_query,
};
use crate::api::ApiErr;
use crate::api::extractors::{StoreScopedUser, key_grants_store_permission};
use crate::state::PgAppState;

/// Get payments for an invoice.
///
/// User must be a member of the store the invoice belongs to.
#[utoipa::path(
    get,
    path = "/invoices/{invoice_id}/payments",
    tag = "invoices",
    security(("bearer_auth" = [])),
    params(
        ("invoice_id" = String, Path, description = "Invoice ID")
    ),
    responses(
        (status = 200, description = "List of payments", body = Vec<PaymentResponse>),
        (status = 401, description = "Unauthorized"),
        (status = 404, description = "Invoice not found"),
    )
)]
pub async fn get_invoice_payments<A>(
    StoreScopedUser(user, key_scope): StoreScopedUser,
    State(state): State<PgAppState<A>>,
    Path(invoice_id): Path<String>,
) -> Result<Json<Vec<PaymentResponse>>, StatusCode>
where
    A: SessionService + 'static,
{
    let id = InvoiceId::from_string(invoice_id);

    // Verify permission
    let _invoice = get_invoice_with_permission(&state, &user, key_scope.as_deref(), &id).await?;

    let payments = PaymentReader::get_for_invoice(&*state.data_service, &id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(payments.into_iter().map(|p| p.into()).collect()))
}

/// List payments with optional filters.
///
/// Users can only see payments for stores they are members of.
/// store_id is required unless user is a server admin.
#[utoipa::path(
    get,
    path = "/payments",
    tag = "payments",
    security(("bearer_auth" = [])),
    params(ListPaymentsQuery),
    responses(
        (status = 200, description = "List of payments", body = PaymentListResponse),
        (status = 400, description = "invalid status filter"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Not a member of the store"),
    )
)]
pub async fn list_payments<A>(
    StoreScopedUser(user, key_scope): StoreScopedUser,
    State(state): State<PgAppState<A>>,
    Query(query): Query<ListPaymentsQuery>,
) -> Result<Json<PaymentListResponse>, ApiErr>
where
    A: SessionService + 'static,
{
    // Resolve the store scope once: one store (membership-checked), the
    // caller's own stores, or the whole server for an admin. The distinction is
    // load-bearing - a nil-UUID sentinel here was once an authorization hole.
    let scope = verify_store_access_for_query(&*state.data_service, &user, query.store_id).await?;
    // A payment has no permission of its own; it's read under its invoice's
    // `canviewinvoices`, same as every other payment/invoice read in this file.
    let scope = narrow_scope_by_key(scope, key_scope.as_deref(), VIEW_INVOICES)?;

    // The same builder the CSV export uses, so the two cannot answer different
    // questions - an export that ignores a filter the list applied downloads
    // something other than what is on screen. It applies the store
    // scope first and ANDs every filter onto it; only StoreScope::All leaves
    // the query unfiltered, and only an admin gets it.
    let mut params =
        build_payment_filter_params(&scope, query.status.as_deref(), query.search.as_deref())?;

    if let Some(limit) = query.limit {
        params = params.with_limit(limit);
    }

    if let Some(offset) = query.offset {
        params = params.with_offset(offset);
    }

    let (total, payments) = PaymentReader::query(&*state.data_service, &params)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // A payment has no store of its own — it inherits its invoice's. With no
    // store filter this page can span stores, so resolve the owning store for
    // each row so the client can label it. Deduplicated by invoice:
    // several payments against one invoice cost a single lookup.
    // Map each payment to its store.
    //
    // A scoped query needs no lookups at all: the WHERE clause already
    // guaranteed every row belongs to the store the caller named, so asking the
    // database to rediscover it costs up to `limit` round trips for an answer we
    // were handed. Only the admin all-stores page genuinely has to ask - and it
    // asks for every distinct invoice concurrently rather than one after
    // another, so the cost is one round trip of latency instead of N.
    // Only StoreScope::One can skip the lookups: it is the single case where
    // every row is known to share one store. Membership scoping spans stores
    // just like the admin view, so it has to ask.
    let store_of_invoice: std::collections::HashMap<String, ::types::StoreId> = match &scope {
        StoreScope::One(store_id) => payments
            .iter()
            .map(|payment| (payment.invoice_id.0.clone(), *store_id))
            .collect(),
        StoreScope::Membership(_) | StoreScope::All => {
            let unique: std::collections::BTreeSet<&str> = payments
                .iter()
                .map(|payment| payment.invoice_id.0.as_str())
                .collect();

            let lookups = unique.into_iter().map(|id| {
                let invoice_id = ::types::InvoiceId(id.to_string());
                let data_service = &state.data_service;
                async move {
                    // A payment whose invoice cannot be read just goes
                    // unlabelled; that is not worth failing the whole list over.
                    match InvoiceReader::get(&**data_service, &invoice_id).await {
                        Ok(Some(invoice)) => Some((invoice_id.0, invoice.store_id)),
                        Ok(None) => None,
                        Err(e) => {
                            tracing::warn!(
                                invoice_id = %invoice_id.0,
                                error = %e,
                                "could not read invoice while labelling payments; \
                                 row will show a bare store id"
                            );
                            None
                        }
                    }
                }
            });

            futures::future::join_all(lookups)
                .await
                .into_iter()
                .flatten()
                .collect()
        }
    };
    let store_names = resolve_store_names(&state, store_of_invoice.values().copied()).await;

    let payments = payments
        .into_iter()
        .map(|p| {
            let store_id = store_of_invoice.get(&p.invoice_id.0).copied();
            let mut response: PaymentResponse = p.into();
            response.store_name = store_id.and_then(|s| store_names.get(&s.0).cloned());
            response.store_id = store_id.map(|s| s.0.to_string());
            response
        })
        .collect();

    Ok(Json(PaymentListResponse { total, payments }))
}

/// Get a single payment by ID.
///
/// User must be a member of the store the payment's invoice belongs to.
#[utoipa::path(
    get,
    path = "/payments/{payment_id}",
    tag = "payments",
    security(("bearer_auth" = [])),
    params(
        ("payment_id" = Uuid, Path, description = "Payment ID")
    ),
    responses(
        (status = 200, description = "Payment details", body = PaymentResponse),
        (status = 401, description = "Unauthorized"),
        (status = 404, description = "Payment not found"),
    )
)]
pub async fn get_payment<A>(
    StoreScopedUser(user, key_scope): StoreScopedUser,
    State(state): State<PgAppState<A>>,
    Path(payment_id): Path<Uuid>,
) -> Result<Json<PaymentResponse>, StatusCode>
where
    A: SessionService + 'static,
{
    let payment = PaymentReader::get(&*state.data_service, payment_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    // Look up the invoice to check store membership
    let invoice = InvoiceReader::get(&*state.data_service, &payment.invoice_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    if user.role != auth::Role::ServerAdmin {
        let is_member = state
            .data_service
            .get_user_store(user.id, invoice.store_id)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
            .is_some();

        if !is_member {
            return Err(StatusCode::NOT_FOUND);
        }
    }

    if !key_grants_store_permission(key_scope.as_deref(), VIEW_INVOICES, invoice.store_id) {
        return Err(StatusCode::NOT_FOUND);
    }

    // The invoice is already in hand from the membership check above, so
    // labelling the payment costs one store lookup rather than a second
    // invoice round trip. Without this, `store_id`/`store_name` were always
    // null here even though the list endpoint fills them in for the exact
    // same rows - documented, but a schema field that only some routes honour
    // is a trap for anyone who reads it from `GET /payments/{id}` directly.
    let store_names = resolve_store_names(&state, std::iter::once(invoice.store_id)).await;
    let mut response: PaymentResponse = payment.into();
    response.store_id = Some(invoice.store_id.0.to_string());
    response.store_name = store_names.get(&invoice.store_id.0).cloned();

    Ok(Json(response))
}

/// Get detailed status of an invoice including payments.
///
/// User must be a member of the store the invoice belongs to.
#[utoipa::path(
    get,
    path = "/invoices/{invoice_id}/status",
    tag = "invoices",
    security(("bearer_auth" = [])),
    params(
        ("invoice_id" = String, Path, description = "Invoice ID")
    ),
    responses(
        (status = 200, description = "Invoice status with payment details", body = InvoiceStatusResponse),
        (status = 401, description = "Unauthorized"),
        (status = 404, description = "Invoice not found"),
    )
)]
pub async fn get_invoice_status<A>(
    StoreScopedUser(user, key_scope): StoreScopedUser,
    State(state): State<PgAppState<A>>,
    Path(invoice_id): Path<String>,
) -> Result<Json<InvoiceStatusResponse>, StatusCode>
where
    A: SessionService + 'static,
{
    let id = InvoiceId::from_string(invoice_id);

    let invoice = get_invoice_with_permission(&state, &user, key_scope.as_deref(), &id).await?;

    let payments = PaymentReader::get_for_invoice(&*state.data_service, &id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let options = PaymentOptionReader::get_for_invoice(&*state.data_service, &id)
        .await
        .unwrap_or_default();

    let now = chrono::Utc::now();
    let confirmed_count = payments.iter().filter(|p| p.confirmed_at.is_some()).count();

    Ok(Json(InvoiceStatusResponse {
        id: invoice.id.0,
        status: invoice.status,
        amount: invoice.amount.clone(),
        amount_received: invoice.amount_received,
        currency: invoice.currency,
        expires_at: invoice.expires_at,
        payment_count: payments.len(),
        confirmed_count,
        is_paid: invoice.status == InvoiceStatus::Paid,
        is_expired: invoice.status == InvoiceStatus::Expired || invoice.expires_at < now,
        payment_options: options.into_iter().map(Into::into).collect(),
        payments: payments.into_iter().map(|p| p.into()).collect(),
    }))
}

/// The shortfall a settlement tolerance accepted on an invoice, and the
/// setting that allowed it.
#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct SettlementAllowanceResponse {
    pub invoice_id: String,
    pub shortfall: String,
    pub tolerance_percent: String,
    /// `"store"` or `"default"`.
    pub source: String,
    pub recorded_at: String,
}

/// Get the tolerance allowance recorded for an invoice, if it settled short.
#[utoipa::path(
    get,
    path = "/invoices/{invoice_id}/settlement-allowance",
    tag = "invoices",
    security(("bearer_auth" = [])),
    params(
        ("invoice_id" = String, Path, description = "Invoice ID")
    ),
    responses(
        (status = 200, description = "The recorded allowance, or null if the invoice settled in full", body = Option<SettlementAllowanceResponse>),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Insufficient permissions"),
        (status = 404, description = "Invoice not found"),
    )
)]
pub async fn get_invoice_settlement_allowance<A>(
    StoreScopedUser(user, key_scope): StoreScopedUser,
    State(state): State<PgAppState<A>>,
    Path(invoice_id): Path<String>,
) -> Result<Json<Option<SettlementAllowanceResponse>>, StatusCode>
where
    A: SessionService + 'static,
{
    let id = InvoiceId::from_string(invoice_id);
    get_invoice_with_permission(&state, &user, key_scope.as_deref(), &id).await?;

    let allowance = data_service::SettlementToleranceReader::get_settlement_allowance(
        &*state.data_service,
        &id,
    )
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(allowance.map(|a| SettlementAllowanceResponse {
        invoice_id: a.invoice_id,
        shortfall: a.shortfall,
        tolerance_percent: a.tolerance_percent,
        source: a.source,
        recorded_at: a.recorded_at.to_rfc3339(),
    })))
}
