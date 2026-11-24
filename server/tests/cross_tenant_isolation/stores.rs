//! Stores: get-by-id and list, across tenants.
//!
//! Review finding, checked: `get_store` and `list_stores` are mounted at
//! `GET /stores/{store_id}` and `GET /stores/` (`server/src/api/mod.rs`) -
//! not orphaned handlers.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use uuid::Uuid;

use server::api::{AuthenticatedUser, StoreScopedUser};

use crate::support::{app_state, seed_tenant, service, store_scoped_user_info, user_info};

#[tokio::test]
#[ignore]
async fn get_store_by_id_across_tenants_is_refused() {
    let pg = service().await;
    let a = seed_tenant(&pg, "a").await;
    let b = seed_tenant(&pg, "b").await;
    let state = app_state(Arc::new(pg));

    let result = server::api::stores::get_store(
        AuthenticatedUser(user_info(a.user_id)),
        State(state.clone()),
        Path(b.store.id.0),
    )
    .await;
    assert_eq!(
        result.unwrap_err(),
        StatusCode::NOT_FOUND,
        "A must not be able to fetch B's store by id"
    );

    // A foreign store must be indistinguishable from one that does not exist.
    let missing = server::api::stores::get_store(
        AuthenticatedUser(user_info(a.user_id)),
        State(state.clone()),
        Path(Uuid::new_v4()),
    )
    .await;
    assert_eq!(
        missing.unwrap_err(),
        StatusCode::NOT_FOUND,
        "a missing store answers the same as a foreign one"
    );

    // So must delete.
    let foreign_delete = server::api::stores::delete_store(
        store_scoped_user_info(a.user_id),
        State(state.clone()),
        Path(b.store.id.0),
    )
    .await;
    let missing_delete = server::api::stores::delete_store(
        store_scoped_user_info(a.user_id),
        State(state.clone()),
        Path(Uuid::new_v4()),
    )
    .await;
    assert_eq!(foreign_delete.unwrap_err(), StatusCode::NOT_FOUND);
    assert_eq!(missing_delete.unwrap_err(), StatusCode::NOT_FOUND);

    // Positive control: without this, an endpoint that refuses regardless of
    // caller would pass the assertion above for the wrong reason.
    let own = server::api::stores::get_store(
        AuthenticatedUser(user_info(a.user_id)),
        State(state),
        Path(a.store.id.0),
    )
    .await
    .expect("A must be able to fetch A's own store by id");
    assert_eq!(own.id, a.store.id.0);
}

#[tokio::test]
#[ignore]
async fn list_stores_never_includes_another_tenants_store() {
    let pg = service().await;
    let a = seed_tenant(&pg, "a").await;
    let b = seed_tenant(&pg, "b").await;
    let state = app_state(Arc::new(pg));

    let result = server::api::stores::list_stores(
        AuthenticatedUser(user_info(a.user_id)),
        State(state),
        Query(Default::default()),
    )
    .await
    .expect("listing one's own stores must succeed");

    let ids: Vec<Uuid> = result.iter().map(|s| s.id).collect();
    assert!(ids.contains(&a.store.id.0), "A's own store must be listed");
    assert!(
        !ids.contains(&b.store.id.0),
        "B's store leaked into A's store list"
    );
}

/// The default listing hides an archived store; the opt-in shows it.
#[tokio::test]
#[ignore]
async fn list_stores_hides_archived_unless_asked() {
    use auth::repository::StoreRepository;
    use server::api::stores::ListStoresQuery;

    let pg = service().await;
    let a = seed_tenant(&pg, "a").await;
    pg.archive_store(a.store.id).await.expect("archive");
    let state = app_state(Arc::new(pg));

    let default = server::api::stores::list_stores(
        AuthenticatedUser(user_info(a.user_id)),
        State(state.clone()),
        Query(ListStoresQuery::default()),
    )
    .await
    .expect("list");
    assert!(
        !default.iter().any(|s| s.id == a.store.id.0),
        "an archived store must not appear in the default listing"
    );

    let all = server::api::stores::list_stores(
        AuthenticatedUser(user_info(a.user_id)),
        State(state),
        Query(ListStoresQuery { archived: true }),
    )
    .await
    .expect("list");
    let found = all
        .iter()
        .find(|s| s.id == a.store.id.0)
        .expect("archived=true must list the archived store");
    assert!(found.archived);
}

/// Unarchive: refused for a non-owner, restores the store for the owner, and
/// lets the store back into the default listing.
#[tokio::test]
#[ignore]
async fn unarchive_store_is_owner_only_and_round_trips() {
    use auth::repository::StoreRepository;
    use server::api::stores::ListStoresQuery;

    let pg = service().await;
    let a = seed_tenant(&pg, "a").await;
    let b = seed_tenant(&pg, "b").await;
    pg.archive_store(a.store.id).await.expect("archive");
    let state = app_state(Arc::new(pg));

    let refused = server::api::stores::unarchive_store(
        store_scoped_user_info(b.user_id),
        State(state.clone()),
        Path(a.store.id.0),
    )
    .await;
    assert_eq!(refused.unwrap_err(), StatusCode::NOT_FOUND);
    let missing = server::api::stores::unarchive_store(
        store_scoped_user_info(b.user_id),
        State(state.clone()),
        Path(Uuid::new_v4()),
    )
    .await;
    assert_eq!(
        missing.unwrap_err(),
        StatusCode::NOT_FOUND,
        "a missing store answers the same as a foreign one"
    );
    let still = state
        .data_service
        .get_store(a.store.id)
        .await
        .unwrap()
        .unwrap();
    assert!(still.archived, "a refused unarchive must change nothing");

    let restored = server::api::stores::unarchive_store(
        store_scoped_user_info(a.user_id),
        State(state.clone()),
        Path(a.store.id.0),
    )
    .await
    .expect("owner may unarchive");
    assert!(!restored.archived);
    let persisted = state
        .data_service
        .get_store(a.store.id)
        .await
        .unwrap()
        .unwrap();
    assert!(!persisted.archived, "unarchive must persist");

    let listed = server::api::stores::list_stores(
        AuthenticatedUser(user_info(a.user_id)),
        State(state),
        Query(ListStoresQuery::default()),
    )
    .await
    .expect("list");
    assert!(listed.iter().any(|s| s.id == a.store.id.0));
}

/// An archived store refuses new invoices with 409 `store_archived`; the same
/// store, once unarchived, gets past that guard (and fails later, for want of
/// a wallet, which proves the 409 was the archive and not a blanket refusal).
#[tokio::test]
#[ignore]
async fn archived_store_refuses_new_invoices() {
    use auth::repository::StoreRepository;
    use axum::Json;
    use server::api::AuthenticatedCaller;
    use server::api::invoices::{CreateInvoiceRequest, create_invoice};

    let pg = service().await;
    let a = seed_tenant(&pg, "a").await;
    let state = app_state(Arc::new(pg));
    let request = || CreateInvoiceRequest {
        store_id: a.store.id.0,
        currency: "USD".to_string(),
        amount: "10.00".to_string(),
        expiration_seconds: None,
        metadata: None,
        customer_email: None,
        webhook_url: None,
        redirect_url: None,
    };
    let caller = || AuthenticatedCaller {
        user: user_info(a.user_id),
        is_operator: false,
        key_scope: None,
    };

    state.data_service.archive_store(a.store.id).await.unwrap();
    let Err((status, Json(body))) =
        create_invoice(caller(), State(state.clone()), Json(request())).await
    else {
        panic!("an archived store must refuse invoice creation");
    };
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"], "store_archived");

    let mut store = state
        .data_service
        .get_store(a.store.id)
        .await
        .unwrap()
        .unwrap();
    store.archived = false;
    state.data_service.update_store(&store).await.unwrap();
    let Err((status, Json(body))) = create_invoice(caller(), State(state), Json(request())).await
    else {
        panic!("seeded store has no wallet, so creation should still fail downstream");
    };
    assert_ne!(
        body["error"], "store_archived",
        "unarchived store: {body:?}"
    );
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// Key-scope boundary: delete_store and unarchive_store check store ownership
// but, through AuthenticatedUser, ignore any API-key scope. Fixed to also
// require ethpay.store.canmodifystoresettings, the one Owner-exclusive
// permission already in the schema - matching the owner-only semantics these
// two handlers already enforce by role.

#[tokio::test]
#[ignore]
async fn a_key_scoped_to_modify_store_settings_can_delete_and_unarchive_its_store() {
    let pg = service().await;
    let a = seed_tenant(&pg, "a").await;
    let state = app_state(Arc::new(pg));

    let scope = Some(vec![auth::Policies::STORE_MODIFY_SETTINGS.to_string()]);
    server::api::stores::delete_store(
        StoreScopedUser(user_info(a.user_id), scope.clone()),
        State(state.clone()),
        Path(a.store.id.0),
    )
    .await
    .expect("a key scoped to canmodifystoresettings must be admitted to delete its own store");

    server::api::stores::unarchive_store(
        StoreScopedUser(user_info(a.user_id), scope),
        State(state),
        Path(a.store.id.0),
    )
    .await
    .expect("a key scoped to canmodifystoresettings must be admitted to unarchive its own store");
}

/// The refusal side: a key scoped to an unrelated permission must be refused,
/// even for the real owner acting on their own store.
#[tokio::test]
#[ignore]
async fn a_key_scoped_to_create_invoices_is_refused_deleting_or_unarchiving_its_store() {
    let pg = service().await;
    let a = seed_tenant(&pg, "a").await;
    let state = app_state(Arc::new(pg));

    let scope = Some(vec![auth::Policies::STORE_CREATE_INVOICE.to_string()]);
    let delete_result = server::api::stores::delete_store(
        StoreScopedUser(user_info(a.user_id), scope.clone()),
        State(state.clone()),
        Path(a.store.id.0),
    )
    .await;
    assert_eq!(
        delete_result.unwrap_err(),
        StatusCode::NOT_FOUND,
        "a key scoped only to cancreateinvoice must be refused deleting its own store"
    );

    let unarchive_result = server::api::stores::unarchive_store(
        StoreScopedUser(user_info(a.user_id), scope),
        State(state),
        Path(a.store.id.0),
    )
    .await;
    assert_eq!(
        unarchive_result.unwrap_err(),
        StatusCode::NOT_FOUND,
        "a key scoped only to cancreateinvoice must be refused unarchiving its own store"
    );
}
