//! Payouts, refunds, and webhook deliveries: the same store-membership shape
//! as invoices and payments (a path id checked with `get_user_store`, then the
//! row itself matched to that store), so the same nil/foreign/admin questions
//! apply and had no coverage at all before this test.
//!
//! Review finding, checked: calling `get_payout`/`list_payouts`,
//! `list_refunds`, and `list_deliveries_for_invoice`/`list_deliveries_for_store`
//! directly proves the handler's tenancy logic but not that an HTTP request
//! reaches it. All five are mounted routes (`server/src/api/mod.rs`):
//! `GET /{store_id}/payouts`, `GET /{store_id}/payouts/{payout_id}`,
//! `GET /{invoice_id}/refunds`, `GET /{store_id}/webhook-deliveries`, and
//! `GET /{invoice_id}/webhook-deliveries` - not orphaned handlers.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use uuid::Uuid;

use server::api::StoreScopedUser;
use server::api::payouts::{CreatePayoutRequest, SettlePayoutRequest};
use types::ChainId;

use crate::support::{
    app_state, seed_confirmed_payment, seed_payout, seed_refund, seed_tenant,
    seed_webhook_delivery, service, store_scoped_user_info, user_info, user_info_with_role,
};

#[tokio::test]
#[ignore]
async fn payout_endpoints_refuse_a_non_members_store() {
    let pg = service().await;
    let a = seed_tenant(&pg, "a").await;
    let b = seed_tenant(&pg, "b").await;
    let a_payout = seed_payout(&pg, &a.store).await;
    let b_payout = seed_payout(&pg, &b.store).await;
    let state = app_state(Arc::new(pg));

    let get_result = server::api::payouts::get_payout(
        store_scoped_user_info(a.user_id),
        State(state.clone()),
        Path((b.store.id.0, b_payout)),
    )
    .await;
    assert_eq!(
        get_result.unwrap_err(),
        StatusCode::NOT_FOUND,
        "A must not be able to fetch a payout on B's store"
    );

    let list_result = server::api::payouts::list_payouts(
        store_scoped_user_info(a.user_id),
        State(state.clone()),
        Path(b.store.id.0),
    )
    .await;
    assert_eq!(
        list_result.unwrap_err(),
        StatusCode::NOT_FOUND,
        "A must not be able to list payouts on B's store"
    );

    // Positive control: without this, both endpoints refusing every caller,
    // including one asking about their own store, would pass the assertions
    // above for the wrong reason.
    let own = server::api::payouts::get_payout(
        store_scoped_user_info(a.user_id),
        State(state.clone()),
        Path((a.store.id.0, a_payout)),
    )
    .await
    .expect("A must be able to fetch a payout on A's own store");
    assert_eq!(own.id, a_payout);

    let own_list = server::api::payouts::list_payouts(
        store_scoped_user_info(a.user_id),
        State(state),
        Path(a.store.id.0),
    )
    .await
    .expect("A must be able to list payouts on A's own store");
    assert!(own_list.payouts.iter().any(|p| p.id == a_payout));
}

/// The "id from B passed directly to a detail endpoint" case: A names A's own
/// store, so the membership gate passes, but supplies B's payout id. The
/// membership check alone must not be enough - the payout itself has to be
/// matched to the store named in the path, the same as every other
/// `*_for_store` lookup in this file.
#[tokio::test]
#[ignore]
async fn get_payout_refuses_another_tenants_payout_even_via_the_callers_own_store() {
    let pg = service().await;
    let a = seed_tenant(&pg, "a").await;
    let b = seed_tenant(&pg, "b").await;
    let b_payout = seed_payout(&pg, &b.store).await;
    let state = app_state(Arc::new(pg));

    let result = server::api::payouts::get_payout(
        store_scoped_user_info(a.user_id),
        State(state),
        Path((a.store.id.0, b_payout)),
    )
    .await;

    assert_eq!(
        result.unwrap_err(),
        StatusCode::NOT_FOUND,
        "A must not be able to fetch B's payout by naming A's own store and B's payout id"
    );
}

#[tokio::test]
#[ignore]
async fn list_refunds_across_tenants_is_refused() {
    let pg = service().await;
    let a = seed_tenant(&pg, "a").await;
    let b = seed_tenant(&pg, "b").await;
    let a_refund = seed_refund(&pg, &a).await;
    let _b_refund = seed_refund(&pg, &b).await;
    let state = app_state(Arc::new(pg));

    let result = server::api::refunds::list_refunds(
        store_scoped_user_info(a.user_id),
        State(state.clone()),
        Path(b.invoice.id.0.clone()),
    )
    .await;
    assert_eq!(
        result.unwrap_err(),
        StatusCode::NOT_FOUND,
        "A must not be able to list refunds on B's invoice"
    );

    // Positive control: without this, an endpoint that 404s regardless of
    // caller would pass the assertion above for the wrong reason.
    let own = server::api::refunds::list_refunds(
        store_scoped_user_info(a.user_id),
        State(state),
        Path(a.invoice.id.0.clone()),
    )
    .await
    .expect("A must be able to list refunds on A's own invoice");
    assert!(own.iter().any(|r| r.id == a_refund));
}

#[tokio::test]
#[ignore]
async fn list_deliveries_for_invoice_across_tenants_is_refused() {
    let pg = service().await;
    let a = seed_tenant(&pg, "a").await;
    let b = seed_tenant(&pg, "b").await;
    let a_delivery = seed_webhook_delivery(&pg, &a).await;
    let _b_delivery = seed_webhook_delivery(&pg, &b).await;
    let state = app_state(Arc::new(pg));

    let result = server::api::webhook_deliveries::list_deliveries_for_invoice(
        store_scoped_user_info(a.user_id),
        State(state.clone()),
        Path(b.invoice.id.0.clone()),
    )
    .await;
    assert_eq!(
        result.unwrap_err(),
        StatusCode::NOT_FOUND,
        "A must not be able to list webhook deliveries on B's invoice"
    );

    // Positive control: without this, an endpoint that 404s regardless of
    // caller would pass the assertion above for the wrong reason.
    let own = server::api::webhook_deliveries::list_deliveries_for_invoice(
        store_scoped_user_info(a.user_id),
        State(state),
        Path(a.invoice.id.0.clone()),
    )
    .await
    .expect("A must be able to list webhook deliveries on A's own invoice");
    assert!(own.deliveries.iter().any(|d| d.id == a_delivery));
}

#[tokio::test]
#[ignore]
async fn list_deliveries_for_store_across_tenants_is_refused() {
    let pg = service().await;
    let a = seed_tenant(&pg, "a").await;
    let b = seed_tenant(&pg, "b").await;
    let a_delivery = seed_webhook_delivery(&pg, &a).await;
    let _b_delivery = seed_webhook_delivery(&pg, &b).await;
    let state = app_state(Arc::new(pg));

    let result = server::api::webhook_deliveries::list_deliveries_for_store(
        store_scoped_user_info(a.user_id),
        State(state.clone()),
        Path(b.store.id.0),
    )
    .await;
    assert_eq!(
        result.unwrap_err(),
        StatusCode::NOT_FOUND,
        "A must not be able to list webhook deliveries on B's store"
    );

    // Positive control: without this, an endpoint that 404s regardless of
    // caller would pass the assertion above for the wrong reason.
    let own = server::api::webhook_deliveries::list_deliveries_for_store(
        store_scoped_user_info(a.user_id),
        State(state),
        Path(a.store.id.0),
    )
    .await
    .expect("A must be able to list webhook deliveries on A's own store");
    assert!(own.deliveries.iter().any(|d| d.id == a_delivery));
}

/// The nil-`store_id` bug shape, for the two endpoints in this section keyed
/// directly by a `store_id` path segment. `list_refunds` and
/// `list_deliveries_for_invoice` are keyed by invoice id (a string, not a
/// UUID) instead, so there is no nil-`store_id` case to construct for them -
/// the admin-bypass tests below cover those two.
#[tokio::test]
#[ignore]
async fn payout_endpoints_with_a_nil_store_id_is_refused_like_any_foreign_store() {
    let pg = service().await;
    let a = seed_tenant(&pg, "a").await;
    let a_payout = seed_payout(&pg, &a.store).await;
    let state = app_state(Arc::new(pg));

    let get_result = server::api::payouts::get_payout(
        store_scoped_user_info(a.user_id),
        State(state.clone()),
        Path((Uuid::nil(), a_payout)),
    )
    .await;
    assert_eq!(
        get_result.unwrap_err(),
        StatusCode::NOT_FOUND,
        "a nil store_id must not be treated as 'every store'"
    );

    let list_result = server::api::payouts::list_payouts(
        store_scoped_user_info(a.user_id),
        State(state),
        Path(Uuid::nil()),
    )
    .await;
    assert_eq!(
        list_result.unwrap_err(),
        StatusCode::NOT_FOUND,
        "a nil store_id must not be treated as 'every store'"
    );
}

/// The admin-bypass side of the same shape: a `ServerAdmin` is not a member
/// of either tenant's store, yet must still reach both - the direction where
/// an over-narrow membership check would wrongly refuse the one role that is
/// supposed to see everything.
#[tokio::test]
#[ignore]
async fn payout_endpoints_as_server_admin_reach_every_tenants_store() {
    let pg = service().await;
    let a = seed_tenant(&pg, "a").await;
    let b = seed_tenant(&pg, "b").await;
    let b_payout = seed_payout(&pg, &b.store).await;
    let state = app_state(Arc::new(pg));

    let get_result = server::api::payouts::get_payout(
        StoreScopedUser(
            user_info_with_role(a.user_id, auth::Role::ServerAdmin),
            None,
        ),
        State(state.clone()),
        Path((b.store.id.0, b_payout)),
    )
    .await
    .expect("a server admin must be able to fetch another tenant's payout");
    assert_eq!(get_result.id, b_payout);

    let list_result = server::api::payouts::list_payouts(
        StoreScopedUser(
            user_info_with_role(a.user_id, auth::Role::ServerAdmin),
            None,
        ),
        State(state),
        Path(b.store.id.0),
    )
    .await
    .expect("a server admin must be able to list another tenant's payouts");
    assert!(list_result.payouts.iter().any(|p| p.id == b_payout));
}

#[tokio::test]
#[ignore]
async fn list_refunds_as_server_admin_reaches_another_tenants_invoice() {
    let pg = service().await;
    let a = seed_tenant(&pg, "a").await;
    let b = seed_tenant(&pg, "b").await;
    let b_refund = seed_refund(&pg, &b).await;
    let state = app_state(Arc::new(pg));

    let result = server::api::refunds::list_refunds(
        StoreScopedUser(
            user_info_with_role(a.user_id, auth::Role::ServerAdmin),
            None,
        ),
        State(state),
        Path(b.invoice.id.0.clone()),
    )
    .await
    .expect("a server admin must be able to list another tenant's refunds");
    assert!(result.iter().any(|r| r.id == b_refund));
}

#[tokio::test]
#[ignore]
async fn list_deliveries_for_invoice_as_server_admin_reaches_another_tenants_invoice() {
    let pg = service().await;
    let a = seed_tenant(&pg, "a").await;
    let b = seed_tenant(&pg, "b").await;
    let b_delivery = seed_webhook_delivery(&pg, &b).await;
    let state = app_state(Arc::new(pg));

    let result = server::api::webhook_deliveries::list_deliveries_for_invoice(
        StoreScopedUser(
            user_info_with_role(a.user_id, auth::Role::ServerAdmin),
            None,
        ),
        State(state),
        Path(b.invoice.id.0.clone()),
    )
    .await
    .expect("a server admin must be able to list another tenant's webhook deliveries");
    assert!(result.deliveries.iter().any(|d| d.id == b_delivery));
}

#[tokio::test]
#[ignore]
async fn list_deliveries_for_store_with_a_nil_store_id_is_refused_like_any_foreign_store() {
    let pg = service().await;
    let a = seed_tenant(&pg, "a").await;
    let state = app_state(Arc::new(pg));

    let result = server::api::webhook_deliveries::list_deliveries_for_store(
        store_scoped_user_info(a.user_id),
        State(state),
        Path(Uuid::nil()),
    )
    .await;
    assert_eq!(
        result.unwrap_err(),
        StatusCode::NOT_FOUND,
        "a nil store_id must not be treated as 'every store'"
    );
}

#[tokio::test]
#[ignore]
async fn list_deliveries_for_store_as_server_admin_reaches_every_tenants_store() {
    let pg = service().await;
    let a = seed_tenant(&pg, "a").await;
    let b = seed_tenant(&pg, "b").await;
    let b_delivery = seed_webhook_delivery(&pg, &b).await;
    let state = app_state(Arc::new(pg));

    let result = server::api::webhook_deliveries::list_deliveries_for_store(
        StoreScopedUser(
            user_info_with_role(a.user_id, auth::Role::ServerAdmin),
            None,
        ),
        State(state),
        Path(b.store.id.0),
    )
    .await
    .expect("a server admin must be able to list another tenant's webhook deliveries by store");
    assert!(result.deliveries.iter().any(|d| d.id == b_delivery));
}

// =============================================================================
// Key-scope boundary: refunds and webhook-delivery endpoints
//
// Review finding: `list_refunds` and all three webhook-delivery handlers
// checked only bare store membership (`get_user_store(...).is_some()`)
// through `AuthenticatedUser`, which carries no API-key scope at all - the
// same gap payouts had. Fixed to gate `list_refunds` behind
// `ethpay.store.canviewpayments` and the three webhook-delivery handlers
// (reads and `replay_delivery`, a write) behind
// `ethpay.store.canmanagewebhooks`. `a`'s own store role (Owner, via
// `seed_tenant`) already holds both permissions, so only the key's scope
// varies below.
// =============================================================================

#[tokio::test]
#[ignore]
async fn a_key_scoped_to_view_payments_can_list_refunds() {
    let pg = service().await;
    let a = seed_tenant(&pg, "a").await;
    let a_refund = seed_refund(&pg, &a).await;
    let state = app_state(Arc::new(pg));

    let scope = Some(vec![auth::Policies::STORE_VIEW_PAYMENTS.to_string()]);
    let result = server::api::refunds::list_refunds(
        StoreScopedUser(user_info(a.user_id), scope),
        State(state),
        Path(a.invoice.id.0.clone()),
    )
    .await
    .expect("a key scoped to canviewpayments must be admitted to list refunds");
    assert!(result.iter().any(|r| r.id == a_refund));
}

/// The refusal side: a key scoped to an unrelated permission must be
/// refused - `canviewpayments` is what the guard checks, not merely "has
/// some permission".
#[tokio::test]
#[ignore]
async fn a_key_scoped_to_create_invoices_is_refused_listing_refunds() {
    let pg = service().await;
    let a = seed_tenant(&pg, "a").await;
    seed_refund(&pg, &a).await;
    let state = app_state(Arc::new(pg));

    let scope = Some(vec![auth::Policies::STORE_CREATE_INVOICE.to_string()]);
    let result = server::api::refunds::list_refunds(
        StoreScopedUser(user_info(a.user_id), scope),
        State(state),
        Path(a.invoice.id.0.clone()),
    )
    .await;
    assert_eq!(
        result.unwrap_err(),
        StatusCode::NOT_FOUND,
        "a key scoped only to cancreateinvoice must be refused listing refunds"
    );
}

#[tokio::test]
#[ignore]
async fn a_key_scoped_to_manage_webhooks_can_list_webhook_deliveries() {
    let pg = service().await;
    let a = seed_tenant(&pg, "a").await;
    let a_delivery = seed_webhook_delivery(&pg, &a).await;
    let state = app_state(Arc::new(pg));

    let scope = Some(vec![auth::Policies::STORE_MANAGE_WEBHOOKS.to_string()]);

    let by_invoice = server::api::webhook_deliveries::list_deliveries_for_invoice(
        StoreScopedUser(user_info(a.user_id), scope.clone()),
        State(state.clone()),
        Path(a.invoice.id.0.clone()),
    )
    .await
    .expect("a key scoped to canmanagewebhooks must be admitted to list deliveries by invoice");
    assert!(by_invoice.deliveries.iter().any(|d| d.id == a_delivery));

    let by_store = server::api::webhook_deliveries::list_deliveries_for_store(
        StoreScopedUser(user_info(a.user_id), scope),
        State(state),
        Path(a.store.id.0),
    )
    .await
    .expect("a key scoped to canmanagewebhooks must be admitted to list deliveries by store");
    assert!(by_store.deliveries.iter().any(|d| d.id == a_delivery));
}

/// The refusal side for both read handlers: `canviewpayments` alone - the
/// permission the sibling `list_refunds`/payout-read endpoints accept - must
/// not be enough here. Webhook delivery history is an operations concern
/// gated behind `canmanagewebhooks`, not merchant-facing payment data.
#[tokio::test]
#[ignore]
async fn a_key_scoped_to_view_payments_is_refused_listing_webhook_deliveries() {
    let pg = service().await;
    let a = seed_tenant(&pg, "a").await;
    seed_webhook_delivery(&pg, &a).await;
    let state = app_state(Arc::new(pg));

    let scope = Some(vec![auth::Policies::STORE_VIEW_PAYMENTS.to_string()]);

    let by_invoice = server::api::webhook_deliveries::list_deliveries_for_invoice(
        StoreScopedUser(user_info(a.user_id), scope.clone()),
        State(state.clone()),
        Path(a.invoice.id.0.clone()),
    )
    .await;
    assert_eq!(
        by_invoice.unwrap_err(),
        StatusCode::NOT_FOUND,
        "a key scoped only to canviewpayments must be refused listing deliveries by invoice"
    );

    let by_store = server::api::webhook_deliveries::list_deliveries_for_store(
        StoreScopedUser(user_info(a.user_id), scope),
        State(state),
        Path(a.store.id.0),
    )
    .await;
    assert_eq!(
        by_store.unwrap_err(),
        StatusCode::NOT_FOUND,
        "a key scoped only to canviewpayments must be refused listing deliveries by store"
    );
}

/// `replay_delivery`'s grant side proves the permission gate specifically,
/// not full delivery - this harness's `app_state` has no webhook sink
/// configured (see `support::app_state`), so a replay that gets past the
/// permission check still ends in `SERVICE_UNAVAILABLE`. That is still a
/// meaningful positive: it is a *different* refusal than the permission
/// gate produces (`NOT_FOUND`, pinned below), proving the gate - not some
/// later check - is what distinguishes grant from refusal.
#[tokio::test]
#[ignore]
async fn a_key_scoped_to_manage_webhooks_reaches_past_the_permission_gate_to_replay_a_delivery() {
    let pg = service().await;
    let a = seed_tenant(&pg, "a").await;
    let a_delivery = seed_webhook_delivery(&pg, &a).await;
    let state = app_state(Arc::new(pg));

    let scope = Some(vec![auth::Policies::STORE_MANAGE_WEBHOOKS.to_string()]);
    let result = server::api::webhook_deliveries::replay_delivery(
        StoreScopedUser(user_info(a.user_id), scope),
        State(state),
        Path((a.store.id.0, a_delivery)),
    )
    .await;
    assert_eq!(
        result.unwrap_err(),
        StatusCode::SERVICE_UNAVAILABLE,
        "a key scoped to canmanagewebhooks must pass the permission gate - the no-sink \
         SERVICE_UNAVAILABLE below it is this test harness's limit, not a refusal"
    );
}

#[tokio::test]
#[ignore]
async fn a_key_scoped_to_view_payments_is_refused_replaying_a_delivery() {
    let pg = service().await;
    let a = seed_tenant(&pg, "a").await;
    let a_delivery = seed_webhook_delivery(&pg, &a).await;
    let state = app_state(Arc::new(pg));

    let scope = Some(vec![auth::Policies::STORE_VIEW_PAYMENTS.to_string()]);
    let result = server::api::webhook_deliveries::replay_delivery(
        StoreScopedUser(user_info(a.user_id), scope),
        State(state),
        Path((a.store.id.0, a_delivery)),
    )
    .await;
    assert_eq!(
        result.unwrap_err(),
        StatusCode::NOT_FOUND,
        "a key scoped only to canviewpayments must be refused replaying a delivery"
    );
}

// =============================================================================
// Key-scope boundary: payout endpoints
//
// Review finding: all five payout handlers checked only bare store
// membership (`get_user_store(...).is_some()`) through `AuthenticatedUser`,
// which carries no API-key scope at all - any role down to Guest, and any
// API key regardless of scope, could create, settle or abandon a payout.
// Fixed to match every sibling module (webhooks, invoices, settings):
// `StoreScopedUser` plus a store permission check
// (`ethpay.store.canmodifyinvoices` for the three writes,
// `ethpay.store.canviewpayments` for the two reads). These tests pin the new
// boundary in both directions; `a`'s own store role (Owner, via
// `seed_tenant`) already holds both permissions, so only the key's scope
// varies.
// =============================================================================

#[tokio::test]
#[ignore]
async fn a_key_scoped_to_modify_invoices_can_create_a_payout() {
    let pg = service().await;
    let a = seed_tenant(&pg, "a").await;
    let amount = seed_confirmed_payment(&pg, &a).await;
    let state = app_state(Arc::new(pg));

    let scope = Some(vec![auth::Policies::STORE_MODIFY_INVOICES.to_string()]);
    let created = server::api::payouts::create_payout(
        StoreScopedUser(user_info(a.user_id), scope),
        State(state),
        Path(a.store.id.0),
        Json(CreatePayoutRequest {
            invoice_ids: vec![a.invoice.id.0.clone()],
            destination_address: format!("0x{:040x}", Uuid::new_v4().as_u128()),
            chain_id: ChainId::evm(11155111),
            asset_symbol: "ETH".to_string(),
            token_address: None,
        }),
    )
    .await
    .expect("a key scoped to canmodifyinvoices must be admitted to create a payout");
    assert_eq!(created.amount, amount.to_string());
}

#[tokio::test]
#[ignore]
async fn a_key_scoped_to_modify_invoices_can_settle_and_abandon_payouts() {
    let pg = service().await;
    let a = seed_tenant(&pg, "a").await;
    let to_settle = seed_payout(&pg, &a.store).await;
    let to_abandon = seed_payout(&pg, &a.store).await;
    let state = app_state(Arc::new(pg));

    let scope = Some(vec![auth::Policies::STORE_MODIFY_INVOICES.to_string()]);

    let settled = server::api::payouts::settle_payout(
        StoreScopedUser(user_info(a.user_id), scope.clone()),
        State(state.clone()),
        Path((a.store.id.0, to_settle)),
        Json(SettlePayoutRequest {
            tx_hash: format!("0x{:064x}", Uuid::new_v4().as_u128()),
        }),
    )
    .await
    .expect("a key scoped to canmodifyinvoices must be admitted to settle a payout");
    assert_eq!(settled.id, to_settle);

    let abandoned = server::api::payouts::abandon_payout(
        StoreScopedUser(user_info(a.user_id), scope),
        State(state),
        Path((a.store.id.0, to_abandon)),
        None,
    )
    .await
    .expect("a key scoped to canmodifyinvoices must be admitted to abandon a payout");
    assert_eq!(abandoned.id, to_abandon);
}

/// The refusal side for all three write handlers: a key scoped only to
/// `canviewpayments` (read-only) must be refused every write, matching
/// Guest's own role permissions exactly - a Guest invited as "read only"
/// must not be able to create, settle or abandon a payout even though the
/// handler used to let any member do so.
#[tokio::test]
#[ignore]
async fn a_key_scoped_to_view_payments_is_refused_creating_settling_or_abandoning_a_payout() {
    let pg = service().await;
    let a = seed_tenant(&pg, "a").await;
    seed_confirmed_payment(&pg, &a).await;
    let existing = seed_payout(&pg, &a.store).await;
    let state = app_state(Arc::new(pg));

    let scope = Some(vec![auth::Policies::STORE_VIEW_PAYMENTS.to_string()]);

    let create_result = server::api::payouts::create_payout(
        StoreScopedUser(user_info(a.user_id), scope.clone()),
        State(state.clone()),
        Path(a.store.id.0),
        Json(CreatePayoutRequest {
            invoice_ids: vec![a.invoice.id.0.clone()],
            destination_address: format!("0x{:040x}", Uuid::new_v4().as_u128()),
            chain_id: ChainId::evm(11155111),
            asset_symbol: "ETH".to_string(),
            token_address: None,
        }),
    )
    .await;
    assert_eq!(
        create_result.unwrap_err(),
        StatusCode::NOT_FOUND,
        "a key scoped only to canviewpayments must be refused creating a payout"
    );

    let settle_result = server::api::payouts::settle_payout(
        StoreScopedUser(user_info(a.user_id), scope.clone()),
        State(state.clone()),
        Path((a.store.id.0, existing)),
        Json(SettlePayoutRequest {
            tx_hash: format!("0x{:064x}", Uuid::new_v4().as_u128()),
        }),
    )
    .await;
    assert_eq!(
        settle_result.unwrap_err(),
        StatusCode::NOT_FOUND,
        "a key scoped only to canviewpayments must be refused settling a payout"
    );

    let abandon_result = server::api::payouts::abandon_payout(
        StoreScopedUser(user_info(a.user_id), scope),
        State(state),
        Path((a.store.id.0, existing)),
        None,
    )
    .await;
    assert_eq!(
        abandon_result.unwrap_err(),
        StatusCode::NOT_FOUND,
        "a key scoped only to canviewpayments must be refused abandoning a payout"
    );
}

#[tokio::test]
#[ignore]
async fn a_key_scoped_to_view_payments_can_get_and_list_payouts() {
    let pg = service().await;
    let a = seed_tenant(&pg, "a").await;
    let a_payout = seed_payout(&pg, &a.store).await;
    let state = app_state(Arc::new(pg));

    let scope = Some(vec![auth::Policies::STORE_VIEW_PAYMENTS.to_string()]);

    let got = server::api::payouts::get_payout(
        StoreScopedUser(user_info(a.user_id), scope.clone()),
        State(state.clone()),
        Path((a.store.id.0, a_payout)),
    )
    .await
    .expect("a key scoped to canviewpayments must be admitted to fetch a payout");
    assert_eq!(got.id, a_payout);

    let listed = server::api::payouts::list_payouts(
        StoreScopedUser(user_info(a.user_id), scope),
        State(state),
        Path(a.store.id.0),
    )
    .await
    .expect("a key scoped to canviewpayments must be admitted to list payouts");
    assert!(listed.payouts.iter().any(|p| p.id == a_payout));
}

/// The refusal side for both read handlers: a key scoped to an unrelated
/// permission (`cancreateinvoice`, no view-payments grant) must be refused -
/// a working guard must distinguish "has some permission" from "has this
/// permission".
#[tokio::test]
#[ignore]
async fn a_key_scoped_to_create_invoices_is_refused_getting_or_listing_payouts() {
    let pg = service().await;
    let a = seed_tenant(&pg, "a").await;
    let a_payout = seed_payout(&pg, &a.store).await;
    let state = app_state(Arc::new(pg));

    let scope = Some(vec![auth::Policies::STORE_CREATE_INVOICE.to_string()]);

    let get_result = server::api::payouts::get_payout(
        StoreScopedUser(user_info(a.user_id), scope.clone()),
        State(state.clone()),
        Path((a.store.id.0, a_payout)),
    )
    .await;
    assert_eq!(
        get_result.unwrap_err(),
        StatusCode::NOT_FOUND,
        "a key scoped only to cancreateinvoice must be refused fetching a payout"
    );

    let list_result = server::api::payouts::list_payouts(
        StoreScopedUser(user_info(a.user_id), scope),
        State(state),
        Path(a.store.id.0),
    )
    .await;
    assert_eq!(
        list_result.unwrap_err(),
        StatusCode::NOT_FOUND,
        "a key scoped only to cancreateinvoice must be refused listing payouts"
    );
}
