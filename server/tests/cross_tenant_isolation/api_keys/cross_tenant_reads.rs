//! An API key must not reach another tenant's data any more than a session
//! can - the negative half of this module's boundary, one test per resource.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use uuid::Uuid;

use server::api::stores::StoreWalletResult;

use crate::support::{
    app_state, authenticate_via_bearer, authenticate_via_bearer_scoped, seed_payout, seed_refund,
    seed_tenant, seed_webhook_delivery, service,
};

/// The payment side of `scope_parity::an_api_key_is_bound_to_its_owners_tenancy_same_as_a_session`.
/// An API key that carried more than its owner's scope would be a distinct
/// bug from session tenancy, so every payment-reading endpoint - not just
/// invoices - needs its own API-key-authenticated check, not just the
/// session-based ones. Positive controls for `get_payment`/
/// `get_invoice_payments`/`get_invoice_status` live in
/// `scope_parity::an_api_keys_own_payment_reads_remain_reachable`, for the
/// same reason the payout/refund/delivery ones do: without them, all three
/// 404ing regardless of whose data was asked for would still pass the
/// assertions below.
#[tokio::test]
#[ignore]
async fn an_api_key_cannot_reach_another_tenants_payments() {
    let pg = service().await;
    let a = seed_tenant(&pg, "a").await;
    let b = seed_tenant(&pg, "b").await;
    let state = app_state(Arc::new(pg));

    let a_via_key = authenticate_via_bearer_scoped(&state, &a.api_key_raw).await;
    let result =
        server::api::invoices::get_payment(a_via_key, State(state.clone()), Path(b.payment_id))
            .await;
    assert_eq!(
        result.unwrap_err(),
        StatusCode::NOT_FOUND,
        "an API key must not fetch another tenant's payment by id"
    );

    let a_via_key = authenticate_via_bearer_scoped(&state, &a.api_key_raw).await;
    let result = server::api::invoices::get_invoice_payments(
        a_via_key,
        State(state.clone()),
        Path(b.invoice.id.0.clone()),
    )
    .await;
    assert_eq!(
        result.unwrap_err(),
        StatusCode::NOT_FOUND,
        "an API key must not list another tenant's invoice's payments"
    );

    let a_via_key = authenticate_via_bearer_scoped(&state, &a.api_key_raw).await;
    let result = server::api::invoices::get_invoice_status(
        a_via_key,
        State(state.clone()),
        Path(b.invoice.id.0.clone()),
    )
    .await;
    assert_eq!(
        result.unwrap_err(),
        StatusCode::NOT_FOUND,
        "an API key must not read another tenant's invoice status"
    );

    let a_via_key = authenticate_via_bearer_scoped(&state, &a.api_key_raw).await;
    let listed = server::api::invoices::list_payments(
        a_via_key,
        State(state),
        Query(server::api::invoices::ListPaymentsQuery {
            store_id: None,
            status: None,
            search: None,
            limit: None,
            offset: None,
        }),
    )
    .await
    .expect("an api key with no store filter must see its owner's payments");

    let ids: Vec<String> = listed.payments.iter().map(|p| p.id.clone()).collect();
    assert!(ids.contains(&a.payment_id.to_string()));
    assert!(
        !ids.contains(&b.payment_id.to_string()),
        "an API key's unfiltered payment listing must not include another tenant's payment"
    );
}

/// The wallet side of the same boundary: a key authenticates as its owner,
/// and the owner's wallet scoping applies exactly as it does to a session.
#[tokio::test]
#[ignore]
async fn an_api_key_cannot_reach_another_tenants_wallets() {
    let pg = service().await;
    let a = seed_tenant(&pg, "a").await;
    let b = seed_tenant(&pg, "b").await;
    let state = app_state(Arc::new(pg));

    let a_via_key = authenticate_via_bearer(&state, &a.api_key_raw).await;
    let listed = server::api::stores::list_wallets(a_via_key, State(state.clone()))
        .await
        .expect("listing one's own wallets via an api key must succeed");
    assert!(
        listed.iter().any(|w| w.id == a.wallet.id),
        "A's own wallet must be listed via an api key"
    );
    assert!(
        !listed.iter().any(|w| w.id == b.wallet.id),
        "B's wallet leaked into A's api-key-authenticated wallet list"
    );

    let a_via_key = authenticate_via_bearer(&state, &a.api_key_raw).await;
    let result =
        server::api::stores::get_wallet_by_id(a_via_key, State(state.clone()), Path(b.wallet.id))
            .await;
    assert_eq!(
        result.unwrap_err(),
        StatusCode::NOT_FOUND,
        "an API key must not fetch another tenant's wallet by id"
    );

    let a_via_key = authenticate_via_bearer_scoped(&state, &a.api_key_raw).await;
    let result = server::api::stores::get_store_wallet(
        a_via_key,
        State(state.clone()),
        Path(b.store.id.0),
        Query(server::api::stores::StoreWalletQuery {
            namespace: None,
            // The bare form: these tests are about who may read a store's
            // wallet at all, not about which wallet a payment method
            // resolves to. Method-scoped resolution has its own tests.
            payment_method_id: None,
        }),
    )
    .await;
    assert_eq!(
        result.err(),
        Some(StatusCode::FORBIDDEN),
        "an API key must not read another tenant's store wallet"
    );

    // Positive control: without this, `get_store_wallet` refusing every
    // caller, API-key included, would pass the assertion above for the
    // wrong reason.
    let a_via_key = authenticate_via_bearer_scoped(&state, &a.api_key_raw).await;
    let own = server::api::stores::get_store_wallet(
        a_via_key,
        State(state),
        Path(a.store.id.0),
        Query(server::api::stores::StoreWalletQuery {
            namespace: None,
            // The bare form: these tests are about who may read a store's
            // wallet at all, not about which wallet a payment method
            // resolves to. Method-scoped resolution has its own tests.
            payment_method_id: None,
        }),
    )
    .await
    .expect("an API key must be able to read its owner's own store wallet");
    let StoreWalletResult::Bare(own) = own else {
        panic!("bare form (no payment_method_id) must resolve to StoreWalletResult::Bare");
    };
    assert_eq!(own.wallet.id, a.wallet.id);
}

#[tokio::test]
#[ignore]
async fn an_api_key_cannot_reach_another_tenants_stores() {
    let pg = service().await;
    let a = seed_tenant(&pg, "a").await;
    let b = seed_tenant(&pg, "b").await;
    let state = app_state(Arc::new(pg));

    let a_via_key = authenticate_via_bearer(&state, &a.api_key_raw).await;
    let result =
        server::api::stores::get_store(a_via_key, State(state.clone()), Path(b.store.id.0)).await;
    assert_eq!(
        result.unwrap_err(),
        StatusCode::NOT_FOUND,
        "an API key must not fetch another tenant's store by id"
    );

    // Positive control: without this, `get_store` refusing every caller,
    // API-key included, would pass the assertion above for the wrong
    // reason.
    let a_via_key = authenticate_via_bearer(&state, &a.api_key_raw).await;
    let own = server::api::stores::get_store(a_via_key, State(state.clone()), Path(a.store.id.0))
        .await
        .expect("an API key must be able to fetch its owner's own store by id");
    assert_eq!(own.id, a.store.id.0);

    let a_via_key = authenticate_via_bearer(&state, &a.api_key_raw).await;
    let listed =
        server::api::stores::list_stores(a_via_key, State(state), Query(Default::default()))
            .await
            .expect("listing one's own stores via an api key must succeed");
    let ids: Vec<Uuid> = listed.iter().map(|s| s.id).collect();
    assert!(
        ids.contains(&a.store.id.0),
        "A's own store must be listed via an api key"
    );
    assert!(
        !ids.contains(&b.store.id.0),
        "B's store leaked into A's api-key-authenticated store list"
    );
}

/// The payout/refund/webhook-delivery side of the same boundary: an API key
/// resolves to its owner, and the owner's store-membership scoping applies
/// exactly as it does to a session. Positive controls for these five
/// endpoints live in `scope_parity::an_api_keys_own_payouts_refunds_and_deliveries_remain_reachable`.
#[tokio::test]
#[ignore]
async fn an_api_key_cannot_reach_another_tenants_payouts_refunds_or_deliveries() {
    let pg = service().await;
    let a = seed_tenant(&pg, "a").await;
    let b = seed_tenant(&pg, "b").await;
    let b_payout = seed_payout(&pg, &b.store).await;
    let _a_refund = seed_refund(&pg, &a).await;
    let _b_refund = seed_refund(&pg, &b).await;
    let _a_delivery = seed_webhook_delivery(&pg, &a).await;
    let _b_delivery = seed_webhook_delivery(&pg, &b).await;
    let state = app_state(Arc::new(pg));

    let a_via_key = authenticate_via_bearer_scoped(&state, &a.api_key_raw).await;
    let result = server::api::payouts::get_payout(
        a_via_key,
        State(state.clone()),
        Path((b.store.id.0, b_payout)),
    )
    .await;
    assert_eq!(
        result.unwrap_err(),
        StatusCode::NOT_FOUND,
        "an API key must not fetch a payout on another tenant's store"
    );

    let a_via_key = authenticate_via_bearer_scoped(&state, &a.api_key_raw).await;
    let result =
        server::api::payouts::list_payouts(a_via_key, State(state.clone()), Path(b.store.id.0))
            .await;
    assert_eq!(
        result.unwrap_err(),
        StatusCode::NOT_FOUND,
        "an API key must not list payouts on another tenant's store"
    );

    let a_via_key = authenticate_via_bearer(&state, &a.api_key_raw).await;
    let result = server::api::refunds::list_refunds(
        a_via_key,
        State(state.clone()),
        Path(b.invoice.id.0.clone()),
    )
    .await;
    assert_eq!(
        result.unwrap_err(),
        StatusCode::NOT_FOUND,
        "an API key must not list refunds on another tenant's invoice"
    );

    let a_via_key = authenticate_via_bearer(&state, &a.api_key_raw).await;
    let result = server::api::webhook_deliveries::list_deliveries_for_invoice(
        a_via_key,
        State(state.clone()),
        Path(b.invoice.id.0.clone()),
    )
    .await;
    assert_eq!(
        result.unwrap_err(),
        StatusCode::NOT_FOUND,
        "an API key must not list webhook deliveries on another tenant's invoice"
    );

    let a_via_key = authenticate_via_bearer(&state, &a.api_key_raw).await;
    let result = server::api::webhook_deliveries::list_deliveries_for_store(
        a_via_key,
        State(state),
        Path(b.store.id.0),
    )
    .await;
    assert_eq!(
        result.unwrap_err(),
        StatusCode::NOT_FOUND,
        "an API key must not list webhook deliveries on another tenant's store"
    );
}
