//! Wallets: list, get-by-id, and the store-wallet binding endpoints
//! (read and configure), across tenants.
//!
//! Review finding, checked: `list_wallets`, `get_wallet_by_id` and
//! `get_store_wallet`/`configure_store_wallet` are mounted at `GET /wallets/`,
//! `GET /wallets/{wallet_id}` and `GET`/`PUT /stores/{store_id}/wallet`
//! (`server/src/api/mod.rs`) - not orphaned handlers.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;

use server::api::stores::{SetStoreWalletRequest, StoreWalletResult};
use server::api::{AuthenticatedUser, StoreScopedUser};

use crate::support::{app_state, seed_tenant, service, store_scoped_user_info, user_info};

#[tokio::test]
#[ignore]
async fn wallets_are_scoped_to_the_owning_account() {
    let pg = service().await;
    let a = seed_tenant(&pg, "a").await;
    let b = seed_tenant(&pg, "b").await;
    let state = app_state(Arc::new(pg));

    let listed = server::api::stores::list_wallets(
        AuthenticatedUser(user_info(a.user_id)),
        State(state.clone()),
    )
    .await
    .expect("listing one's own wallets must succeed");
    assert!(
        listed.iter().any(|w| w.id == a.wallet.id),
        "A's own wallet must be listed"
    );
    assert!(
        !listed.iter().any(|w| w.id == b.wallet.id),
        "B's wallet leaked into A's wallet list"
    );

    let result = server::api::stores::get_wallet_by_id(
        AuthenticatedUser(user_info(a.user_id)),
        State(state.clone()),
        Path(b.wallet.id),
    )
    .await;
    assert_eq!(
        result.unwrap_err(),
        StatusCode::NOT_FOUND,
        "A must not be able to fetch B's wallet by id"
    );

    // Positive control: without this, `get_wallet_by_id` refusing every
    // caller, including one asking about their own wallet, would pass the
    // assertion above for the wrong reason.
    let own = server::api::stores::get_wallet_by_id(
        AuthenticatedUser(user_info(a.user_id)),
        State(state),
        Path(a.wallet.id),
    )
    .await
    .expect("A must be able to fetch A's own wallet by id");
    assert_eq!(own.id, a.wallet.id);
}

#[tokio::test]
#[ignore]
async fn store_wallet_endpoints_refuse_a_non_members_store() {
    let pg = service().await;
    let a = seed_tenant(&pg, "a").await;
    let b = seed_tenant(&pg, "b").await;
    let state = app_state(Arc::new(pg));

    let get_result = server::api::stores::get_store_wallet(
        store_scoped_user_info(a.user_id),
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
        get_result.err(),
        Some(StatusCode::FORBIDDEN),
        "A must not be able to read B's store wallet"
    );

    let configure_result = server::api::stores::configure_store_wallet(
        store_scoped_user_info(a.user_id),
        State(state.clone()),
        Path(b.store.id.0),
        axum::Json(SetStoreWalletRequest {
            wallet_id: a.wallet.id,
        }),
    )
    .await;
    assert_eq!(
        configure_result.unwrap_err(),
        StatusCode::FORBIDDEN,
        "A must not be able to pin a wallet onto B's store"
    );

    // Positive control: without this, `get_store_wallet` refusing every
    // caller, including one asking about their own store, would pass the
    // assertion above for the wrong reason.
    let own = server::api::stores::get_store_wallet(
        store_scoped_user_info(a.user_id),
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
    .expect("A must be able to read A's own store wallet");
    let StoreWalletResult::Bare(own) = own else {
        panic!("bare form (no payment_method_id) must resolve to StoreWalletResult::Bare");
    };
    assert_eq!(own.wallet.id, a.wallet.id);
}

#[tokio::test]
#[ignore]
async fn store_wallet_override_refuses_a_wallet_from_another_account() {
    let pg = service().await;
    let a = seed_tenant(&pg, "a").await;
    let b = seed_tenant(&pg, "b").await;
    let state = app_state(Arc::new(pg));

    // A owns this store, so the permission check passes; the repository is
    // what must refuse pointing it at a wallet from B's account.
    let result = server::api::stores::configure_store_wallet(
        store_scoped_user_info(a.user_id),
        State(state.clone()),
        Path(a.store.id.0),
        axum::Json(SetStoreWalletRequest {
            wallet_id: b.wallet.id,
        }),
    )
    .await;

    assert_eq!(
        result.unwrap_err(),
        StatusCode::NOT_FOUND,
        "A's own store must not be pinnable to B's wallet"
    );

    // Positive control: without this, `configure_store_wallet` refusing
    // every wallet id, including the caller's own, would pass the
    // assertion above for the wrong reason.
    let own = server::api::stores::configure_store_wallet(
        store_scoped_user_info(a.user_id),
        State(state),
        Path(a.store.id.0),
        axum::Json(SetStoreWalletRequest {
            wallet_id: a.wallet.id,
        }),
    )
    .await
    .expect("A must be able to pin A's own store to A's own wallet");
    assert_eq!(own.wallet.id, a.wallet.id);
}

/// `get_wallet_rotations` checked store ownership but, through
/// `AuthenticatedUser`, ignored any API-key scope. Fixed to also require
/// `ethpay.store.canmodifystoresettings`, the one Owner-exclusive permission
/// already in the schema - matching the handler's own existing owner-only
/// semantics (`may_read_rotation_history`).
#[tokio::test]
#[ignore]
async fn wallet_rotation_history_is_owner_only() {
    let pg = service().await;
    let a = seed_tenant(&pg, "a").await;
    let b = seed_tenant(&pg, "b").await;
    let state = app_state(Arc::new(pg));

    let foreign = server::api::users::get_wallet_rotations(
        store_scoped_user_info(b.user_id),
        State(state.clone()),
        Path(a.store.id.0),
    )
    .await;
    assert_eq!(
        foreign.unwrap_err().0,
        StatusCode::NOT_FOUND,
        "B must not be able to read A's wallet rotation history"
    );

    // Positive control: without this, an endpoint that refuses regardless
    // of caller would pass the assertion above for the wrong reason.
    let _ = server::api::users::get_wallet_rotations(
        store_scoped_user_info(a.user_id),
        State(state),
        Path(a.store.id.0),
    )
    .await
    .expect("A must be able to read A's own wallet rotation history");
}

#[tokio::test]
#[ignore]
async fn a_key_scoped_to_modify_store_settings_can_read_wallet_rotation_history() {
    let pg = service().await;
    let a = seed_tenant(&pg, "a").await;
    let state = app_state(Arc::new(pg));

    let scope = Some(vec![auth::Policies::STORE_MODIFY_SETTINGS.to_string()]);
    let _ = server::api::users::get_wallet_rotations(
        StoreScopedUser(user_info(a.user_id), scope),
        State(state),
        Path(a.store.id.0),
    )
    .await
    .expect("a key scoped to canmodifystoresettings must be admitted");
}

/// The refusal side: a key scoped to an unrelated permission must be
/// refused, even for the real owner acting on their own store.
#[tokio::test]
#[ignore]
async fn a_key_scoped_to_create_invoices_is_refused_reading_wallet_rotation_history() {
    let pg = service().await;
    let a = seed_tenant(&pg, "a").await;
    let state = app_state(Arc::new(pg));

    let scope = Some(vec![auth::Policies::STORE_CREATE_INVOICE.to_string()]);
    let refused = server::api::users::get_wallet_rotations(
        StoreScopedUser(user_info(a.user_id), scope),
        State(state),
        Path(a.store.id.0),
    )
    .await;
    assert_eq!(
        refused.unwrap_err().0,
        StatusCode::NOT_FOUND,
        "a key scoped only to cancreateinvoice must be refused"
    );
}
