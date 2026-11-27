use axum::{
    body::Body,
    extract::{Query, State},
    http::StatusCode,
    response::Response,
};
use chrono::Utc;
use futures::StreamExt;
use std::sync::Arc;

use ::types::{
    InvoiceQueryParams, InvoiceReader, InvoiceStatus, PaymentQueryParams, PaymentReader,
};
use auth::SessionService;

use super::{
    ListInvoicesQuery, ListPaymentsQuery, StoreScope, VIEW_INVOICES, narrow_scope_by_key,
    verify_store_access_for_query,
};
use crate::api::ApiErr;
use crate::api::extractors::StoreScopedUser;
use crate::state::PgAppState;

/// Maximum number of rows allowed in a CSV export.
pub(crate) const MAX_EXPORT_ROWS: i64 = 50_000;

/// Page size for streaming CSV data from the database.
pub(crate) const EXPORT_PAGE_SIZE: i64 = 1000;

/// Escape a field value for RFC 4180 CSV output.
pub(crate) fn csv_escape_field(field: &str) -> String {
    if field.contains(',') || field.contains('"') || field.contains('\n') || field.contains('\r') {
        let mut escaped = String::with_capacity(field.len() + 2);
        escaped.push('"');
        for ch in field.chars() {
            if ch == '"' {
                escaped.push('"');
            }
            escaped.push(ch);
        }
        escaped.push('"');
        escaped
    } else {
        field.to_string()
    }
}

/// Build a single CSV row (CRLF-terminated) from field values.
pub(crate) fn csv_row(fields: &[&str]) -> String {
    let mut row = String::new();
    for (i, field) in fields.iter().enumerate() {
        if i > 0 {
            row.push(',');
        }
        row.push_str(&csv_escape_field(field));
    }
    row.push_str("\r\n");
    row
}

/// Build invoice query params from filter fields.
///
/// Shared by `list_invoices` and the export so the button downloads what is on
/// screen. The store scope goes on first and every filter is ANDed onto it -
/// no filter here may ever replace it.
pub(crate) fn build_invoice_filter_params(
    scope: &StoreScope,
    status: Option<&str>,
    currency: Option<&str>,
    search: Option<&str>,
) -> Result<InvoiceQueryParams, ApiErr> {
    let mut params = scope.apply_invoice(InvoiceQueryParams::new());
    if let Some(s) = status {
        // The reason has to distinguish this from every other 400 the caller
        // might see for `/invoices` or `/payments` - the client used to treat
        // any 400 as "no store selected" (see `verify_store_access_for_query`'s
        // history), and a bare status here would put an invalid filter right
        // back into that same bucket.
        let parsed: InvoiceStatus = s
            .parse()
            .map_err(|_| (StatusCode::BAD_REQUEST, "invalid status filter".to_string()))?;
        params = params.with_status(parsed);
    }
    if let Some(c) = currency {
        params = params.with_currency(c.to_string());
    }
    if let Some(q) = search {
        params = params.with_search(q);
    }
    Ok(params)
}

/// Build payment query params from filter fields.
///
/// Shared by `list_payments` and the export, for the same reason as its invoice
/// twin above.
pub(crate) fn build_payment_filter_params(
    scope: &StoreScope,
    status: Option<&str>,
    search: Option<&str>,
) -> Result<PaymentQueryParams, ApiErr> {
    let mut params = scope.apply_payment(PaymentQueryParams::new());
    if let Some(s) = status {
        match s {
            "confirmed" => params = params.with_confirmed(true),
            "pending" => params = params.with_confirmed(false),
            _ => {
                return Err((StatusCode::BAD_REQUEST, "invalid status filter".to_string()).into());
            }
        }
    }
    if let Some(q) = search {
        params = params.with_search(q);
    }
    Ok(params)
}

/// Export invoices as a streaming CSV file.
///
/// Accepts the same query parameters as `list_invoices`. Streams results
/// in pages of 1000 rows to avoid full-result buffering.
#[utoipa::path(
    get,
    path = "/invoices/export.csv",
    tag = "invoices",
    security(("bearer_auth" = [])),
    params(ListInvoicesQuery),
    responses(
        (status = 200, description = "CSV file (text/csv) streamed as an attachment"),
        (status = 400, description = "Invalid status filter"),
        (status = 401, description = "Unauthorized"),
        (status = 413, description = "More rows matched than the export cap allows"),
    )
)]
#[allow(clippy::too_many_lines)] // CSV export: filter assembly + paged stream + row serialization
pub async fn export_invoices_csv<A>(
    StoreScopedUser(user, key_scope): StoreScopedUser,
    State(state): State<PgAppState<A>>,
    Query(query): Query<ListInvoicesQuery>,
) -> Result<Response, ApiErr>
where
    A: SessionService + 'static,
{
    let scope = verify_store_access_for_query(&*state.data_service, &user, query.store_id).await?;
    let scope = narrow_scope_by_key(scope, key_scope.as_deref(), VIEW_INVOICES)?;
    let base_params = build_invoice_filter_params(
        &scope,
        query.status.as_deref(),
        query.currency.as_deref(),
        query.search.as_deref(),
    )?;

    // Count total matching rows.
    let count_params = base_params.clone().with_limit(1).with_offset(0);
    let (total, _) = InvoiceReader::query(&*state.data_service, &count_params)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    if total > MAX_EXPORT_ROWS {
        let body = serde_json::json!({
            "error": "export_too_large",
            "max_rows": MAX_EXPORT_ROWS,
            "matched_rows": total,
        });
        return Response::builder()
            .status(StatusCode::PAYLOAD_TOO_LARGE)
            .header("Content-Type", "application/json")
            .body(Body::from(body.to_string()))
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR.into());
    }

    // "all" covers both the admin's whole-server export and a merchant's
    // across-my-stores one; neither names a single store.
    let store_label = match &scope {
        StoreScope::One(s) => s.0.to_string(),
        _ => "all".to_string(),
    };
    let date = Utc::now().format("%Y%m%d");
    let filename = format!("invoices_{}_{}.csv", store_label, date);

    let header = csv_row(&[
        "id",
        "store_id",
        "status",
        "currency",
        "amount",
        "created_at",
        "expires_at",
        "paid_at",
        "order_number",
        "customer_email",
    ]);
    let header_stream = futures::stream::once(async { Ok::<_, std::convert::Infallible>(header) });

    let ds = Arc::clone(&state.data_service);
    let data_stream =
        futures::stream::unfold((0i64, base_params, ds), |(offset, params, ds)| async move {
            let page = params
                .clone()
                .with_limit(EXPORT_PAGE_SIZE)
                .with_offset(offset);
            let invoices = match InvoiceReader::query(&*ds, &page).await {
                Ok((_, rows)) => rows,
                Err(_) => return None,
            };
            if invoices.is_empty() {
                return None;
            }

            let new_offset = offset + invoices.len() as i64;
            let mut chunk = String::new();
            for inv in &invoices {
                let order_number = inv
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("order_number"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let customer_email = inv
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("customer_email").or_else(|| m.get("buyer_email")))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");

                let created = inv
                    .created_at
                    .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
                let expires = inv
                    .expires_at
                    .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
                let store = inv.store_id.0.to_string();
                let status = inv.status.to_string();

                chunk.push_str(&csv_row(&[
                    &inv.id.0,
                    &store,
                    &status,
                    &inv.currency,
                    &inv.amount,
                    &created,
                    &expires,
                    "", // paid_at — not stored in InvoiceData
                    order_number,
                    customer_email,
                ]));
            }
            Some((
                Ok::<_, std::convert::Infallible>(chunk),
                (new_offset, params, ds),
            ))
        });

    let body = Body::from_stream(header_stream.chain(data_stream));

    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "text/csv; charset=utf-8")
        .header(
            "Content-Disposition",
            format!("attachment; filename=\"{}\"", filename),
        )
        .body(body)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR.into())
}

/// Export payments as a streaming CSV file.
///
/// Accepts the same query parameters as `list_payments`. Streams results
/// in pages of 1000 rows to avoid full-result buffering.
#[utoipa::path(
    get,
    path = "/payments/export.csv",
    tag = "payments",
    security(("bearer_auth" = [])),
    params(ListPaymentsQuery),
    responses(
        (status = 200, description = "CSV file (text/csv) streamed as an attachment"),
        (status = 400, description = "Invalid status filter"),
        (status = 401, description = "Unauthorized"),
        (status = 413, description = "More rows matched than the export cap allows"),
    )
)]
#[allow(clippy::too_many_lines)] // CSV export: filter assembly + paged stream + row serialization
pub async fn export_payments_csv<A>(
    StoreScopedUser(user, key_scope): StoreScopedUser,
    State(state): State<PgAppState<A>>,
    Query(query): Query<ListPaymentsQuery>,
) -> Result<Response, ApiErr>
where
    A: SessionService + 'static,
{
    let scope = verify_store_access_for_query(&*state.data_service, &user, query.store_id).await?;
    let scope = narrow_scope_by_key(scope, key_scope.as_deref(), VIEW_INVOICES)?;
    let base_params =
        build_payment_filter_params(&scope, query.status.as_deref(), query.search.as_deref())?;

    let count_params = base_params.clone().with_limit(1).with_offset(0);
    let (total, _) = PaymentReader::query(&*state.data_service, &count_params)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    if total > MAX_EXPORT_ROWS {
        let body = serde_json::json!({
            "error": "export_too_large",
            "max_rows": MAX_EXPORT_ROWS,
            "matched_rows": total,
        });
        return Response::builder()
            .status(StatusCode::PAYLOAD_TOO_LARGE)
            .header("Content-Type", "application/json")
            .body(Body::from(body.to_string()))
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR.into());
    }

    // "all" covers both the admin's whole-server export and a merchant's
    // across-my-stores one; neither names a single store.
    let store_label = match &scope {
        StoreScope::One(s) => s.0.to_string(),
        _ => "all".to_string(),
    };
    let date = Utc::now().format("%Y%m%d");
    let filename = format!("payments_{}_{}.csv", store_label, date);

    let header = csv_row(&[
        "id",
        "invoice_id",
        "chain_id",
        "tx_hash",
        "from_address",
        "to_address",
        "amount",
        "status",
        "created_at",
        "confirmed_at",
    ]);
    let header_stream = futures::stream::once(async { Ok::<_, std::convert::Infallible>(header) });

    let ds = Arc::clone(&state.data_service);
    let data_stream =
        futures::stream::unfold((0i64, base_params, ds), |(offset, params, ds)| async move {
            let page = params
                .clone()
                .with_limit(EXPORT_PAGE_SIZE)
                .with_offset(offset);
            let payments = match PaymentReader::query(&*ds, &page).await {
                Ok((_, rows)) => rows,
                Err(_) => return None,
            };
            if payments.is_empty() {
                return None;
            }

            let new_offset = offset + payments.len() as i64;
            let mut chunk = String::new();
            for p in &payments {
                let status = if p.reorged {
                    "reorged"
                } else if p.confirmed_at.is_some() {
                    "confirmed"
                } else {
                    "pending"
                };
                let chain = p.chain_id.to_string();
                let detected = p
                    .detected_at
                    .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
                let confirmed = p
                    .confirmed_at
                    .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
                    .unwrap_or_default();

                chunk.push_str(&csv_row(&[
                    &p.id.to_string(),
                    &p.invoice_id.0,
                    &chain,
                    &p.tx_hash,
                    p.from_address.as_deref().unwrap_or(""),
                    "", // to_address — not stored in PaymentData
                    &p.amount,
                    status,
                    &detected,
                    &confirmed,
                ]));
            }
            Some((
                Ok::<_, std::convert::Infallible>(chunk),
                (new_offset, params, ds),
            ))
        });

    let body = Body::from_stream(header_stream.chain(data_stream));

    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "text/csv; charset=utf-8")
        .header(
            "Content-Disposition",
            format!("attachment; filename=\"{}\"", filename),
        )
        .body(body)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR.into())
}
