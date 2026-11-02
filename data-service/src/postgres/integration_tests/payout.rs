//! Postgres-backed tests for `PgDataService`'s payout reads and writes.
//!
//! Handler-level tests cover the API surface, and `payout_claims` covers the
//! claimed-invoice query. Everything else in `postgres::payout` — the INSERT's
//! column order, the `COALESCE` in the status update, and the row-to-
//! `PayoutData` mapping (`db_to_payout_status` among it) — only runs against a
//! real database here, so this is where a column-order or status-mapping
//! mistake would actually show up.

use chrono::Utc;
use uuid::Uuid;

use types::{ChainId, PayoutData, PayoutReader, PayoutStatus, PayoutWriter, StoreId};

use crate::postgres::PgDataService;

use super::{create_test_service, seed_store};

fn test_payout(store_id: StoreId) -> PayoutData {
    PayoutData {
        id: Uuid::new_v4(),
        store_id,
        invoice_ids: vec![format!("inv-{}", Uuid::new_v4())],
        destination_address: format!("0x{:040x}", Uuid::new_v4().as_u128()),
        chain_id: ChainId::evm(1),
        asset_type: "native".to_string(),
        asset_symbol: "ETH".to_string(),
        token_address: None,
        amount: "1000".to_string(),
        tx_hash: None,
        status: PayoutStatus::Pending,
        fee_amount: None,
        error_message: None,
        created_at: Utc::now(),
        confirmed_at: None,
    }
}

async fn create(service: &PgDataService, payout: &PayoutData) {
    PayoutWriter::create_payout(service, payout)
        .await
        .expect("create payout");
}

async fn get(service: &PgDataService, id: Uuid) -> PayoutData {
    PayoutReader::get_payout(service, id)
        .await
        .expect("get payout")
        .expect("payout exists")
}

#[tokio::test]
#[ignore]
async fn create_then_get_round_trips_every_field() {
    let service = create_test_service().await;
    let store = seed_store(&service).await;

    let mut full = test_payout(store);
    full.invoice_ids = vec!["inv-a".to_string(), "inv-b".to_string()];
    full.token_address = Some("0xtoken".to_string());
    full.fee_amount = Some("42".to_string());
    full.tx_hash = Some("0xhash".to_string());
    full.error_message = Some("retrying".to_string());
    create(&service, &full).await;

    let fetched = get(&service, full.id).await;
    assert_eq!(fetched.id, full.id);
    assert_eq!(fetched.store_id, full.store_id);
    assert_eq!(fetched.invoice_ids, full.invoice_ids);
    assert_eq!(fetched.destination_address, full.destination_address);
    assert_eq!(fetched.chain_id, full.chain_id);
    assert_eq!(fetched.asset_type, full.asset_type);
    assert_eq!(fetched.asset_symbol, full.asset_symbol);
    assert_eq!(fetched.token_address, full.token_address);
    assert_eq!(fetched.amount, full.amount);
    assert_eq!(fetched.tx_hash, full.tx_hash);
    assert_eq!(fetched.status, full.status);
    assert_eq!(fetched.fee_amount, full.fee_amount);
    assert_eq!(fetched.error_message, full.error_message);
    assert_eq!(fetched.confirmed_at, full.confirmed_at);

    // Every optional column left `None` round-trips to `None`, not to the
    // previous payout's values — guards the INSERT's column order as much as
    // the `Some` case above does.
    let bare = test_payout(store);
    create(&service, &bare).await;
    let fetched_bare = get(&service, bare.id).await;
    assert_eq!(fetched_bare.token_address, None);
    assert_eq!(fetched_bare.tx_hash, None);
    assert_eq!(fetched_bare.fee_amount, None);
    assert_eq!(fetched_bare.error_message, None);
}

#[tokio::test]
#[ignore]
async fn get_payout_on_an_unknown_id_is_ok_none() {
    let service = create_test_service().await;

    let fetched = PayoutReader::get_payout(&service, Uuid::new_v4())
        .await
        .expect("get payout");

    assert!(fetched.is_none());
}

#[tokio::test]
#[ignore]
async fn get_payouts_for_store_excludes_another_stores_payout() {
    let service = create_test_service().await;
    let mine = seed_store(&service).await;
    let theirs = seed_store(&service).await;

    let mine_payout = test_payout(mine);
    create(&service, &mine_payout).await;
    let their_payout = test_payout(theirs);
    create(&service, &their_payout).await;

    let (count, payouts) = PayoutReader::get_payouts_for_store(&service, mine, 50, 0)
        .await
        .expect("get payouts for store");

    assert_eq!(count, 1);
    assert_eq!(payouts.len(), 1);
    assert_eq!(payouts[0].id, mine_payout.id);
    assert!(payouts.iter().all(|p| p.id != their_payout.id));
}

#[tokio::test]
#[ignore]
async fn update_payout_status_sets_status_and_coalesces_optional_fields() {
    let service = create_test_service().await;
    let store = seed_store(&service).await;

    let mut payout = test_payout(store);
    payout.tx_hash = Some("0xoriginal".to_string());
    payout.fee_amount = Some("10".to_string());
    payout.error_message = Some("original".to_string());
    create(&service, &payout).await;

    // Passing `None` for every optional field keeps what was already there.
    PayoutWriter::update_payout_status(
        &service,
        payout.id,
        PayoutStatus::Broadcasting,
        None,
        None,
        None,
    )
    .await
    .expect("update status");
    let after_none = get(&service, payout.id).await;
    assert_eq!(after_none.status, PayoutStatus::Broadcasting);
    assert_eq!(after_none.tx_hash, Some("0xoriginal".to_string()));
    assert_eq!(after_none.fee_amount, Some("10".to_string()));
    assert_eq!(after_none.error_message, Some("original".to_string()));

    // Passing `Some` overwrites.
    PayoutWriter::update_payout_status(
        &service,
        payout.id,
        PayoutStatus::Failed,
        Some("0xnew"),
        Some("20"),
        Some("boom"),
    )
    .await
    .expect("update status");
    let after_some = get(&service, payout.id).await;
    assert_eq!(after_some.status, PayoutStatus::Failed);
    assert_eq!(after_some.tx_hash, Some("0xnew".to_string()));
    assert_eq!(after_some.fee_amount, Some("20".to_string()));
    assert_eq!(after_some.error_message, Some("boom".to_string()));
}

#[tokio::test]
#[ignore]
async fn confirm_payout_sets_confirmed_status_and_timestamp() {
    let service = create_test_service().await;
    let store = seed_store(&service).await;

    let payout = test_payout(store);
    create(&service, &payout).await;
    assert!(get(&service, payout.id).await.confirmed_at.is_none());

    PayoutWriter::confirm_payout(&service, payout.id)
        .await
        .expect("confirm payout");

    let fetched = get(&service, payout.id).await;
    assert_eq!(fetched.status, PayoutStatus::Confirmed);
    assert!(fetched.confirmed_at.is_some());
}

#[tokio::test]
#[ignore]
async fn get_active_payouts_includes_non_terminal_and_excludes_terminal() {
    let service = create_test_service().await;
    let store = seed_store(&service).await;

    let mut pending = test_payout(store);
    pending.status = PayoutStatus::Pending;
    create(&service, &pending).await;

    let mut broadcasting = test_payout(store);
    broadcasting.status = PayoutStatus::Broadcasting;
    create(&service, &broadcasting).await;

    let mut confirmed = test_payout(store);
    confirmed.status = PayoutStatus::Confirmed;
    create(&service, &confirmed).await;

    let mut failed = test_payout(store);
    failed.status = PayoutStatus::Failed;
    create(&service, &failed).await;

    let active = PayoutReader::get_active_payouts(&service)
        .await
        .expect("get active payouts");
    let active_ids: Vec<Uuid> = active.iter().map(|p| p.id).collect();

    // `get_active_payouts` defines "active" as pending or broadcasting; this
    // is a global query (no store scope), so other tests' rows may be present
    // too — assert membership, not an exact set.
    assert!(
        active_ids.contains(&pending.id),
        "a pending payout still needs monitoring"
    );
    assert!(
        active_ids.contains(&broadcasting.id),
        "a broadcasting payout still needs monitoring"
    );
    assert!(
        !active_ids.contains(&confirmed.id),
        "a confirmed payout is done"
    );
    assert!(!active_ids.contains(&failed.id), "a failed payout is done");
}

#[tokio::test]
#[ignore]
async fn every_payout_status_round_trips_through_the_database() {
    let service = create_test_service().await;
    let store = seed_store(&service).await;

    for status in [
        PayoutStatus::Pending,
        PayoutStatus::Broadcasting,
        PayoutStatus::Confirmed,
        PayoutStatus::Failed,
    ] {
        let mut payout = test_payout(store);
        payout.status = status;
        create(&service, &payout).await;

        let fetched = get(&service, payout.id).await;
        assert_eq!(fetched.status, status, "{status} did not round-trip");
    }
}
