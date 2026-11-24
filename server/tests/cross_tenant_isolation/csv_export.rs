//! CSV export: builds its filter through the same `verify_store_access_for_query`
//! guard as `list_invoices`/`list_payments`, but is a separate handler and a
//! separate response path (a streamed file, not JSON), so the guard being
//! wired to the list endpoint proves nothing about the export one.
//!
//! Review finding, checked: `export_invoices_csv` and `export_payments_csv`
//! are mounted at `GET /invoices/export.csv` and `GET /payments/export.csv`
//! (`server/src/api/mod.rs`) - not orphaned handlers.

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use uuid::Uuid;

use crate::support::{
    app_state, authenticate_via_bearer_scoped, seed_tenant, service, status_of,
    store_scoped_user_info,
};

#[tokio::test]
#[ignore]
async fn export_invoices_csv_with_another_tenants_store_id_is_refused() {
    let pg = service().await;
    let a = seed_tenant(&pg, "a").await;
    let b = seed_tenant(&pg, "b").await;
    let state = app_state(Arc::new(pg));

    let result = server::api::invoices::export_invoices_csv(
        store_scoped_user_info(a.user_id),
        State(state.clone()),
        Query(server::api::invoices::ListInvoicesQuery {
            store_id: Some(b.store.id.0),
            status: None,
            currency: None,
            search: None,
            limit: None,
            offset: None,
        }),
    )
    .await;
    assert_eq!(
        status_of(result),
        StatusCode::FORBIDDEN,
        "A must not be able to export B's store's invoices by naming its id directly"
    );

    // Positive control: without this, an endpoint that refuses every explicit
    // store_id, including the caller's own, would pass the assertion above
    // for the wrong reason.
    let own = server::api::invoices::export_invoices_csv(
        store_scoped_user_info(a.user_id),
        State(state),
        Query(server::api::invoices::ListInvoicesQuery {
            store_id: Some(a.store.id.0),
            status: None,
            currency: None,
            search: None,
            limit: None,
            offset: None,
        }),
    )
    .await
    .expect("A must be able to export A's own store's invoices by naming its id directly");
    assert_eq!(own.status(), StatusCode::OK);
}

/// The same nil-UUID regression `list_invoices` guards against, but for the
/// export handler's own copy of the store-access check.
#[tokio::test]
#[ignore]
async fn export_invoices_csv_with_a_nil_store_id_is_refused_like_any_foreign_store() {
    let pg = service().await;
    let a = seed_tenant(&pg, "a").await;
    let _b = seed_tenant(&pg, "b").await;
    let state = app_state(Arc::new(pg));

    let result = server::api::invoices::export_invoices_csv(
        store_scoped_user_info(a.user_id),
        State(state),
        Query(server::api::invoices::ListInvoicesQuery {
            store_id: Some(Uuid::nil()),
            status: None,
            currency: None,
            search: None,
            limit: None,
            offset: None,
        }),
    )
    .await;

    assert_eq!(
        status_of(result),
        StatusCode::FORBIDDEN,
        "a nil store_id must not be treated as 'every store'"
    );
}

#[tokio::test]
#[ignore]
async fn export_payments_csv_with_another_tenants_store_id_is_refused() {
    let pg = service().await;
    let a = seed_tenant(&pg, "a").await;
    let b = seed_tenant(&pg, "b").await;
    let state = app_state(Arc::new(pg));

    let result = server::api::invoices::export_payments_csv(
        store_scoped_user_info(a.user_id),
        State(state.clone()),
        Query(server::api::invoices::ListPaymentsQuery {
            store_id: Some(b.store.id.0),
            status: None,
            search: None,
            limit: None,
            offset: None,
        }),
    )
    .await;
    assert_eq!(
        status_of(result),
        StatusCode::FORBIDDEN,
        "A must not be able to export B's store's payments by naming its id directly"
    );

    // Positive control, same reasoning as the invoice export above.
    let own = server::api::invoices::export_payments_csv(
        store_scoped_user_info(a.user_id),
        State(state),
        Query(server::api::invoices::ListPaymentsQuery {
            store_id: Some(a.store.id.0),
            status: None,
            search: None,
            limit: None,
            offset: None,
        }),
    )
    .await
    .expect("A must be able to export A's own store's payments by naming its id directly");
    assert_eq!(own.status(), StatusCode::OK);
}

#[tokio::test]
#[ignore]
async fn export_payments_csv_with_a_nil_store_id_is_refused_like_any_foreign_store() {
    let pg = service().await;
    let a = seed_tenant(&pg, "a").await;
    let _b = seed_tenant(&pg, "b").await;
    let state = app_state(Arc::new(pg));

    let result = server::api::invoices::export_payments_csv(
        store_scoped_user_info(a.user_id),
        State(state),
        Query(server::api::invoices::ListPaymentsQuery {
            store_id: Some(Uuid::nil()),
            status: None,
            search: None,
            limit: None,
            offset: None,
        }),
    )
    .await;

    assert_eq!(
        status_of(result),
        StatusCode::FORBIDDEN,
        "a nil store_id must not be treated as 'every store'"
    );
}

/// Both export handlers take `StoreScopedUser`, so an API key's own
/// permission scope narrows what it can reach here too - this is the export
/// side of the same "carries less than its owner's scope" check
/// `api_keys::cross_tenant_reads` runs for other resources. This suite only
/// grants the default/unrestricted scope, so it still exercises tenant
/// isolation rather than per-key narrowing.
#[tokio::test]
#[ignore]
async fn csv_export_via_api_key_cannot_reach_another_tenants_store() {
    let pg = service().await;
    let a = seed_tenant(&pg, "a").await;
    let b = seed_tenant(&pg, "b").await;
    let state = app_state(Arc::new(pg));

    let a_via_key = authenticate_via_bearer_scoped(&state, &a.api_key_raw).await;
    let result = server::api::invoices::export_invoices_csv(
        a_via_key,
        State(state.clone()),
        Query(server::api::invoices::ListInvoicesQuery {
            store_id: Some(b.store.id.0),
            status: None,
            currency: None,
            search: None,
            limit: None,
            offset: None,
        }),
    )
    .await;
    assert_eq!(
        status_of(result),
        StatusCode::FORBIDDEN,
        "an API key must not export B's store's invoices by naming its id directly"
    );

    // Positive control: without this, an endpoint that refuses every API-key
    // caller, including the owner's own store, would pass the assertion above
    // for the wrong reason.
    let a_via_key = authenticate_via_bearer_scoped(&state, &a.api_key_raw).await;
    let own = server::api::invoices::export_invoices_csv(
        a_via_key,
        State(state.clone()),
        Query(server::api::invoices::ListInvoicesQuery {
            store_id: Some(a.store.id.0),
            status: None,
            currency: None,
            search: None,
            limit: None,
            offset: None,
        }),
    )
    .await
    .expect("an API key must be able to export its owner's own store's invoices");
    assert_eq!(own.status(), StatusCode::OK);

    let a_via_key = authenticate_via_bearer_scoped(&state, &a.api_key_raw).await;
    let result = server::api::invoices::export_payments_csv(
        a_via_key,
        State(state.clone()),
        Query(server::api::invoices::ListPaymentsQuery {
            store_id: Some(b.store.id.0),
            status: None,
            search: None,
            limit: None,
            offset: None,
        }),
    )
    .await;
    assert_eq!(
        status_of(result),
        StatusCode::FORBIDDEN,
        "an API key must not export B's store's payments by naming its id directly"
    );

    // Positive control, same reasoning as above.
    let a_via_key = authenticate_via_bearer_scoped(&state, &a.api_key_raw).await;
    let own = server::api::invoices::export_payments_csv(
        a_via_key,
        State(state),
        Query(server::api::invoices::ListPaymentsQuery {
            store_id: Some(a.store.id.0),
            status: None,
            search: None,
            limit: None,
            offset: None,
        }),
    )
    .await
    .expect("an API key must be able to export its owner's own store's payments");
    assert_eq!(own.status(), StatusCode::OK);
}

/// The ledger export is a separate handler with its own copy of the store
/// guard, so it needs its own proof rather than inheriting the two above: a
/// handler that refuses every explicit store id would satisfy a one-sided
/// assertion for the wrong reason, which is why each case carries a positive
/// control against the caller's own store.
#[tokio::test]
#[ignore]
async fn export_ledger_csv_is_store_scoped_in_both_directions() {
    let pg = service().await;
    let a = seed_tenant(&pg, "a").await;
    let b = seed_tenant(&pg, "b").await;
    let state = app_state(Arc::new(pg));

    let result = server::api::invoices::export_ledger_csv(
        store_scoped_user_info(a.user_id),
        State(state.clone()),
        Query(server::api::invoices::ListPaymentsQuery {
            store_id: Some(b.store.id.0),
            status: None,
            search: None,
            limit: None,
            offset: None,
        }),
    )
    .await;
    assert_eq!(
        status_of(result),
        StatusCode::FORBIDDEN,
        "A must not be able to export B's store's ledger by naming its id directly"
    );

    let own = server::api::invoices::export_ledger_csv(
        store_scoped_user_info(a.user_id),
        State(state),
        Query(server::api::invoices::ListPaymentsQuery {
            store_id: Some(a.store.id.0),
            status: None,
            search: None,
            limit: None,
            offset: None,
        }),
    )
    .await
    .expect("A must be able to export A's own store's ledger by naming its id directly");
    assert_eq!(own.status(), StatusCode::OK);
}
