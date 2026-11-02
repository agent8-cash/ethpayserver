//! Postgres-backed tests for `PgDataService`'s refund reads and writes.
//!
//! Mirrors `payout`: handler-level tests cover the API surface, so this
//! covers the INSERT's column order, the `COALESCE` in the status update,
//! and the row-to-`RefundData` mapping (`db_to_refund_status` among it) —
//! none of which is exercisable against a mock.

use chrono::Utc;
use uuid::Uuid;

use types::{
    ChainId, InvoiceId, InvoiceWriter, PaymentWriter, RefundData, RefundReader, RefundStatus,
    RefundWriter, StoreId,
};

use crate::postgres::PgDataService;

use super::{create_test_service, seeded_test_invoice, test_payment};

/// `refunds.payment_id` is `REFERENCES payments(id)`, and `payments.invoice_id`
/// in turn references an invoice bound to a store — so a refund needs a real
/// invoice and payment seeded first, not just a store.
async fn seed_payment(service: &PgDataService) -> (InvoiceId, StoreId, Uuid) {
    let invoice = seeded_test_invoice(service).await;
    InvoiceWriter::upsert(service, &invoice)
        .await
        .expect("seed invoice");
    let payment = test_payment(&invoice.id);
    PaymentWriter::upsert(service, &payment)
        .await
        .expect("seed payment");
    (invoice.id.clone(), invoice.store_id, payment.id)
}

fn test_refund(invoice_id: InvoiceId, store_id: StoreId, payment_id: Uuid) -> RefundData {
    RefundData {
        id: Uuid::new_v4(),
        invoice_id,
        payment_id,
        store_id,
        to_address: format!("0x{:040x}", Uuid::new_v4().as_u128()),
        chain_id: ChainId::evm(1),
        asset_type: "native".to_string(),
        asset_symbol: "ETH".to_string(),
        token_address: None,
        amount: "1000".to_string(),
        tx_hash: None,
        status: RefundStatus::Pending,
        fee_amount: None,
        reason: None,
        error_message: None,
        created_at: Utc::now(),
        confirmed_at: None,
    }
}

async fn create(service: &PgDataService, refund: &RefundData) {
    RefundWriter::create_refund(service, refund)
        .await
        .expect("create refund");
}

async fn get(service: &PgDataService, id: Uuid) -> RefundData {
    RefundReader::get_refund(service, id)
        .await
        .expect("get refund")
        .expect("refund exists")
}

#[tokio::test]
#[ignore]
async fn create_then_get_round_trips_every_field() {
    let service = create_test_service().await;
    let (invoice_id, store, payment_id) = seed_payment(&service).await;

    let mut full = test_refund(invoice_id.clone(), store, payment_id);
    full.token_address = Some("0xtoken".to_string());
    full.fee_amount = Some("42".to_string());
    full.tx_hash = Some("0xhash".to_string());
    full.reason = Some("customer requested".to_string());
    full.error_message = Some("retrying".to_string());
    create(&service, &full).await;

    let fetched = get(&service, full.id).await;
    assert_eq!(fetched.id, full.id);
    assert_eq!(fetched.invoice_id, full.invoice_id);
    assert_eq!(fetched.payment_id, full.payment_id);
    assert_eq!(fetched.store_id, full.store_id);
    assert_eq!(fetched.to_address, full.to_address);
    assert_eq!(fetched.chain_id, full.chain_id);
    assert_eq!(fetched.asset_type, full.asset_type);
    assert_eq!(fetched.asset_symbol, full.asset_symbol);
    assert_eq!(fetched.token_address, full.token_address);
    assert_eq!(fetched.amount, full.amount);
    assert_eq!(fetched.tx_hash, full.tx_hash);
    assert_eq!(fetched.status, full.status);
    assert_eq!(fetched.fee_amount, full.fee_amount);
    assert_eq!(fetched.reason, full.reason);
    assert_eq!(fetched.error_message, full.error_message);
    assert_eq!(fetched.confirmed_at, full.confirmed_at);

    // Every optional column left `None` round-trips to `None`.
    let bare = test_refund(invoice_id, store, payment_id);
    create(&service, &bare).await;
    let fetched_bare = get(&service, bare.id).await;
    assert_eq!(fetched_bare.token_address, None);
    assert_eq!(fetched_bare.tx_hash, None);
    assert_eq!(fetched_bare.fee_amount, None);
    assert_eq!(fetched_bare.reason, None);
    assert_eq!(fetched_bare.error_message, None);
}

#[tokio::test]
#[ignore]
async fn get_refund_on_an_unknown_id_is_ok_none() {
    let service = create_test_service().await;

    let fetched = RefundReader::get_refund(&service, Uuid::new_v4())
        .await
        .expect("get refund");

    assert!(fetched.is_none());
}

#[tokio::test]
#[ignore]
async fn get_refunds_for_invoice_returns_only_that_invoices_refunds() {
    let service = create_test_service().await;
    let (invoice_id, store, payment_id) = seed_payment(&service).await;
    let (other_invoice_id, other_store, other_payment_id) = seed_payment(&service).await;

    let mine = test_refund(invoice_id.clone(), store, payment_id);
    create(&service, &mine).await;
    let theirs = test_refund(other_invoice_id, other_store, other_payment_id);
    create(&service, &theirs).await;

    let refunds = RefundReader::get_refunds_for_invoice(&service, &invoice_id)
        .await
        .expect("get refunds for invoice");

    assert_eq!(refunds.len(), 1);
    assert_eq!(refunds[0].id, mine.id);
    assert!(refunds.iter().all(|r| r.id != theirs.id));
}

#[tokio::test]
#[ignore]
async fn get_refunds_for_store_excludes_another_stores_refund() {
    let service = create_test_service().await;
    let (mine_invoice, mine_store, mine_payment) = seed_payment(&service).await;
    let (their_invoice, their_store, their_payment) = seed_payment(&service).await;

    let mine = test_refund(mine_invoice, mine_store, mine_payment);
    create(&service, &mine).await;
    let theirs = test_refund(their_invoice, their_store, their_payment);
    create(&service, &theirs).await;

    let (count, refunds) = RefundReader::get_refunds_for_store(&service, mine_store, 50, 0)
        .await
        .expect("get refunds for store");

    assert_eq!(count, 1);
    assert_eq!(refunds.len(), 1);
    assert_eq!(refunds[0].id, mine.id);
    assert!(refunds.iter().all(|r| r.id != theirs.id));
}

#[tokio::test]
#[ignore]
async fn update_refund_status_sets_status_and_coalesces_optional_fields() {
    let service = create_test_service().await;
    let (invoice_id, store, payment_id) = seed_payment(&service).await;

    let mut refund = test_refund(invoice_id, store, payment_id);
    refund.tx_hash = Some("0xoriginal".to_string());
    refund.fee_amount = Some("10".to_string());
    refund.error_message = Some("original".to_string());
    create(&service, &refund).await;

    // Passing `None` for every optional field keeps what was already there.
    RefundWriter::update_refund_status(
        &service,
        refund.id,
        RefundStatus::Broadcasting,
        None,
        None,
        None,
    )
    .await
    .expect("update status");
    let after_none = get(&service, refund.id).await;
    assert_eq!(after_none.status, RefundStatus::Broadcasting);
    assert_eq!(after_none.tx_hash, Some("0xoriginal".to_string()));
    assert_eq!(after_none.fee_amount, Some("10".to_string()));
    assert_eq!(after_none.error_message, Some("original".to_string()));

    // Passing `Some` overwrites.
    RefundWriter::update_refund_status(
        &service,
        refund.id,
        RefundStatus::Failed,
        Some("0xnew"),
        Some("20"),
        Some("boom"),
    )
    .await
    .expect("update status");
    let after_some = get(&service, refund.id).await;
    assert_eq!(after_some.status, RefundStatus::Failed);
    assert_eq!(after_some.tx_hash, Some("0xnew".to_string()));
    assert_eq!(after_some.fee_amount, Some("20".to_string()));
    assert_eq!(after_some.error_message, Some("boom".to_string()));
}

#[tokio::test]
#[ignore]
async fn confirm_refund_sets_confirmed_status_and_timestamp() {
    let service = create_test_service().await;
    let (invoice_id, store, payment_id) = seed_payment(&service).await;

    let refund = test_refund(invoice_id, store, payment_id);
    create(&service, &refund).await;
    assert!(get(&service, refund.id).await.confirmed_at.is_none());

    RefundWriter::confirm_refund(&service, refund.id)
        .await
        .expect("confirm refund");

    let fetched = get(&service, refund.id).await;
    assert_eq!(fetched.status, RefundStatus::Confirmed);
    assert!(fetched.confirmed_at.is_some());
}

#[tokio::test]
#[ignore]
async fn get_active_refunds_includes_non_terminal_and_excludes_terminal() {
    let service = create_test_service().await;
    let (invoice_id, store, payment_id) = seed_payment(&service).await;

    let mut pending = test_refund(invoice_id.clone(), store, payment_id);
    pending.status = RefundStatus::Pending;
    create(&service, &pending).await;

    let mut broadcasting = test_refund(invoice_id.clone(), store, payment_id);
    broadcasting.status = RefundStatus::Broadcasting;
    create(&service, &broadcasting).await;

    let mut confirmed = test_refund(invoice_id.clone(), store, payment_id);
    confirmed.status = RefundStatus::Confirmed;
    create(&service, &confirmed).await;

    let mut failed = test_refund(invoice_id, store, payment_id);
    failed.status = RefundStatus::Failed;
    create(&service, &failed).await;

    let active = RefundReader::get_active_refunds(&service)
        .await
        .expect("get active refunds");
    let active_ids: Vec<Uuid> = active.iter().map(|r| r.id).collect();

    // `get_active_refunds` defines "active" as pending or broadcasting; this
    // is a global query (no store scope), so other tests' rows may be present
    // too — assert membership, not an exact set.
    assert!(
        active_ids.contains(&pending.id),
        "a pending refund still needs monitoring"
    );
    assert!(
        active_ids.contains(&broadcasting.id),
        "a broadcasting refund still needs monitoring"
    );
    assert!(
        !active_ids.contains(&confirmed.id),
        "a confirmed refund is done"
    );
    assert!(!active_ids.contains(&failed.id), "a failed refund is done");
}

#[tokio::test]
#[ignore]
async fn every_refund_status_round_trips_through_the_database() {
    let service = create_test_service().await;
    let (invoice_id, store, payment_id) = seed_payment(&service).await;

    for status in [
        RefundStatus::Pending,
        RefundStatus::Broadcasting,
        RefundStatus::Confirmed,
        RefundStatus::Failed,
    ] {
        let mut refund = test_refund(invoice_id.clone(), store, payment_id);
        refund.status = status;
        create(&service, &refund).await;

        let fetched = get(&service, refund.id).await;
        assert_eq!(fetched.status, status, "{status} did not round-trip");
    }
}
