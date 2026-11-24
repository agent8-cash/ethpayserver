//! Positive controls: an API key must reach exactly what its owner's session
//! reaches - no more (see `cross_tenant_reads`) and no less.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;

use auth::UserId;

use crate::support::{
    app_state, authenticate_via_bearer, authenticate_via_bearer_scoped, seed_payout, seed_refund,
    seed_tenant, seed_webhook_delivery, service,
};

#[tokio::test]
#[ignore]
async fn an_api_key_is_bound_to_its_owners_tenancy_same_as_a_session() {
    let pg = service().await;
    let a = seed_tenant(&pg, "a").await;
    let b = seed_tenant(&pg, "b").await;
    let state = app_state(Arc::new(pg));

    let a_via_key = authenticate_via_bearer_scoped(&state, &a.api_key_raw).await;
    assert_eq!(
        a_via_key.0.id,
        UserId(a.user_id),
        "the api key must resolve to its own owner"
    );

    let result = server::api::invoices::get_invoice(
        a_via_key,
        State(state.clone()),
        Path(b.invoice.id.0.clone()),
    )
    .await;
    assert_eq!(
        result.unwrap_err(),
        StatusCode::NOT_FOUND,
        "an API key must not reach another tenant's invoice any more than a session can"
    );

    // Positive control: without this, `get_invoice` refusing every caller,
    // API-key included, would pass the assertion above for the wrong reason.
    let a_via_key = authenticate_via_bearer_scoped(&state, &a.api_key_raw).await;
    let own = server::api::invoices::get_invoice(
        a_via_key,
        State(state.clone()),
        Path(a.invoice.id.0.clone()),
    )
    .await
    .expect("an API key must be able to fetch its owner's own invoice by id");
    assert_eq!(own.id, a.invoice.id.0);

    let a_via_key = authenticate_via_bearer_scoped(&state, &a.api_key_raw).await;
    let listed = server::api::invoices::list_invoices(
        a_via_key,
        State(state),
        Query(server::api::invoices::ListInvoicesQuery {
            store_id: None,
            status: None,
            currency: None,
            search: None,
            limit: None,
            offset: None,
        }),
    )
    .await
    .expect("an api key with no store filter must see its owner's invoices");

    let ids: Vec<String> = listed.invoices.iter().map(|i| i.id.clone()).collect();
    assert!(ids.contains(&a.invoice.id.0));
    assert!(
        !ids.contains(&b.invoice.id.0),
        "an API key's unfiltered listing must not include another tenant's invoice"
    );
}

/// Positive controls for `cross_tenant_reads::an_api_key_cannot_reach_another_tenants_payments`'s
/// `get_payment`/`get_invoice_payments`/`get_invoice_status` assertions:
/// without these, all three 404ing regardless of whose data was asked for
/// would still pass that test's negative assertions.
#[tokio::test]
#[ignore]
async fn an_api_keys_own_payment_reads_remain_reachable() {
    let pg = service().await;
    let a = seed_tenant(&pg, "a").await;
    let state = app_state(Arc::new(pg));

    let a_via_key = authenticate_via_bearer_scoped(&state, &a.api_key_raw).await;
    let own =
        server::api::invoices::get_payment(a_via_key, State(state.clone()), Path(a.payment_id))
            .await
            .expect("an API key must be able to fetch its owner's own payment by id");
    assert_eq!(own.id, a.payment_id.to_string());

    let a_via_key = authenticate_via_bearer_scoped(&state, &a.api_key_raw).await;
    let own = server::api::invoices::get_invoice_payments(
        a_via_key,
        State(state.clone()),
        Path(a.invoice.id.0.clone()),
    )
    .await
    .expect("an API key must be able to list payments on its owner's own invoice");
    assert!(own.iter().any(|p| p.id == a.payment_id.to_string()));

    let a_via_key = authenticate_via_bearer_scoped(&state, &a.api_key_raw).await;
    let own = server::api::invoices::get_invoice_status(
        a_via_key,
        State(state),
        Path(a.invoice.id.0.clone()),
    )
    .await
    .expect("an API key must be able to read its owner's own invoice status");
    assert_eq!(own.id, a.invoice.id.0);
}

/// Positive controls for `cross_tenant_reads::an_api_key_cannot_reach_another_tenants_payouts_refunds_or_deliveries`:
/// without these, any of its five endpoints refusing every caller, API-key
/// included, would pass its negative assertion for the wrong reason.
#[tokio::test]
#[ignore]
async fn an_api_keys_own_payouts_refunds_and_deliveries_remain_reachable() {
    let pg = service().await;
    let a = seed_tenant(&pg, "a").await;
    let a_payout = seed_payout(&pg, &a.store).await;
    let a_refund = seed_refund(&pg, &a).await;
    let a_delivery = seed_webhook_delivery(&pg, &a).await;
    let state = app_state(Arc::new(pg));

    let a_via_key = authenticate_via_bearer_scoped(&state, &a.api_key_raw).await;
    let own_payout = server::api::payouts::get_payout(
        a_via_key,
        State(state.clone()),
        Path((a.store.id.0, a_payout)),
    )
    .await
    .expect("an API key must be able to fetch its owner's own payout");
    assert_eq!(own_payout.id, a_payout);

    let a_via_key = authenticate_via_bearer_scoped(&state, &a.api_key_raw).await;
    let own_payouts =
        server::api::payouts::list_payouts(a_via_key, State(state.clone()), Path(a.store.id.0))
            .await
            .expect("an API key must be able to list its owner's own payouts");
    assert!(own_payouts.payouts.iter().any(|p| p.id == a_payout));

    let a_via_key = authenticate_via_bearer_scoped(&state, &a.api_key_raw).await;
    let own_refunds = server::api::refunds::list_refunds(
        a_via_key,
        State(state.clone()),
        Path(a.invoice.id.0.clone()),
    )
    .await
    .expect("an API key must be able to list its owner's own refunds");
    assert!(own_refunds.iter().any(|r| r.id == a_refund));

    let a_via_key = authenticate_via_bearer_scoped(&state, &a.api_key_raw).await;
    let own_invoice_deliveries = server::api::webhook_deliveries::list_deliveries_for_invoice(
        a_via_key,
        State(state.clone()),
        Path(a.invoice.id.0.clone()),
    )
    .await
    .expect("an API key must be able to list its owner's own invoice's webhook deliveries");
    assert!(
        own_invoice_deliveries
            .deliveries
            .iter()
            .any(|d| d.id == a_delivery)
    );

    let a_via_key = authenticate_via_bearer_scoped(&state, &a.api_key_raw).await;
    let own_store_deliveries = server::api::webhook_deliveries::list_deliveries_for_store(
        a_via_key,
        State(state),
        Path(a.store.id.0),
    )
    .await
    .expect("an API key must be able to list its owner's own store's webhook deliveries");
    assert!(
        own_store_deliveries
            .deliveries
            .iter()
            .any(|d| d.id == a_delivery)
    );
}
