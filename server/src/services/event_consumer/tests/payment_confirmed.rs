#![allow(clippy::unwrap_used, clippy::expect_used)]

use chrono::Utc;
use data_service::InMemoryDataService;
use evm::monitor::bridge::MemoryBridge;
use evm::monitor::events::PaymentConfirmed;
use evm::{Address, B256, U256};
use std::sync::Arc;
use types::ChainId;
use types::{
    InvoiceData, InvoiceId, InvoiceReader, InvoiceStatus, InvoiceWriter, PaymentData,
    PaymentReader, PaymentWriter, StoreId,
};
use uuid::Uuid;

use super::helpers::{
    MockEVMMonitor, MockEmailSender, RecordingWebhookSink, create_test_consumer,
    create_test_consumer_with_webhook,
};
use crate::services::event_consumer::EventConsumer;

#[tokio::test]
async fn test_handle_payment_confirmed_transitions_to_paid() {
    let ds = Arc::new(InMemoryDataService::new());
    let bridge = Arc::new(MemoryBridge::new());
    let consumer = create_test_consumer(ds.clone(), bridge.clone());

    let invoice_id = InvoiceId::new();
    let store_id = StoreId::new();

    // Create invoice in processing state with full amount received
    let invoice = InvoiceData {
        id: invoice_id.clone(),
        store_id,
        currency: "ETH".to_string(),
        status: InvoiceStatus::Processing,
        amount: "1000000000000000000".to_string(), // 1 ETH
        amount_received: "1000000000000000000".to_string(), // 1 ETH received
        created_at: Utc::now(),
        expires_at: Utc::now() + chrono::Duration::hours(1),
        metadata: None,
        customer_email: None,
        extra: None,
    };
    InvoiceWriter::upsert(&*ds, &invoice).await.unwrap();

    // Create a payment record
    let tx_hash = B256::repeat_byte(0xab);
    let payment = PaymentData {
        id: Uuid::new_v4(),
        invoice_id: invoice_id.clone(),
        payment_option_id: None,
        chain_id: ChainId::parse("eip155:1").unwrap(),
        asset_type: types::AssetType::Native,
        amount: "1000000000000000000".to_string(),
        asset_symbol: "ETH".to_string(),
        token_address: None,
        tx_hash: format!("{:#x}", tx_hash),
        block_number: Some(12345678),
        detected_at: Utc::now(),
        confirmed_at: None,
        from_address: Some("0xabababababababababababababababababababab".to_string()),
        reorged: false,
        extra: None,
        credited_amount: Some("1".to_string()), // 1 ETH
        rate_used: None,
        rate_applied_at: None,
    };
    PaymentWriter::upsert(&*ds, &payment).await.unwrap();

    // Create PaymentConfirmed event
    let event = PaymentConfirmed {
        tx_index: 0,
        chain_id: 1,
        invoice_id: uuid::Uuid::parse_str(invoice_id.as_str()).unwrap(),
        payment_address: Address::ZERO,
        amount: U256::from(1000000000000000000u64),
        tx_hash,
        block_number: 12345678,
        confirmations: 12,
        confirmed_at: Utc::now(),
    };

    // Handle the event
    consumer.handle_payment_confirmed(event).await.unwrap();

    // Verify invoice was marked as paid
    let invoice = InvoiceReader::get(&*ds, &invoice_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(invoice.status, InvoiceStatus::Paid);

    // Verify payment was marked as confirmed
    let payments = PaymentReader::get_for_invoice(&*ds, &invoice_id)
        .await
        .unwrap();
    assert!(payments[0].confirmed_at.is_some());
}

/// `handle_payment_confirmed` has no transactional outbox of its own the
/// way `handle_payment_detected` does - it calls `queue_webhook` directly,
/// mid-handler. A redelivery (the outbox is at-least-once by design; see
/// `resume_after_uncommitted_cursor.rs` for the crash window that produces
/// one) re-runs this whole function for an event already fully applied.
/// What stops a second webhook here is not an id-keyed dedup table but the
/// invoice status check a few lines up: once the first run has already
/// transitioned the invoice to `Paid`, a second run reads that committed
/// status back and falls into the "final state, skip" branch before it ever
/// reaches `queue_webhook` again. This calls the handler twice directly,
/// with no `apply_envelope` guard anywhere in the picture, to prove that
/// holds rather than assume it from reading the match arms.
#[tokio::test]
async fn redelivering_an_already_applied_confirmation_does_not_queue_a_second_webhook() {
    let ds = Arc::new(InMemoryDataService::new());
    let bridge = Arc::new(MemoryBridge::new());
    let store_id = StoreId::new();
    ds.set_webhook(store_id.0, "https://merchant.example/hook", "secret");
    let sink = Arc::new(RecordingWebhookSink::new());
    let consumer = create_test_consumer_with_webhook(ds.clone(), bridge.clone(), sink.clone());

    let invoice_id = InvoiceId::new();
    let invoice = InvoiceData {
        id: invoice_id.clone(),
        store_id,
        currency: "ETH".to_string(),
        status: InvoiceStatus::Processing,
        amount: "1000000000000000000".to_string(),
        amount_received: "1000000000000000000".to_string(),
        created_at: Utc::now(),
        expires_at: Utc::now() + chrono::Duration::hours(1),
        metadata: None,
        customer_email: None,
        extra: None,
    };
    InvoiceWriter::upsert(&*ds, &invoice).await.unwrap();

    let tx_hash = B256::repeat_byte(0xcd);
    let payment = PaymentData {
        id: Uuid::new_v4(),
        invoice_id: invoice_id.clone(),
        payment_option_id: None,
        chain_id: ChainId::parse("eip155:1").unwrap(),
        asset_type: types::AssetType::Native,
        amount: "1000000000000000000".to_string(),
        asset_symbol: "ETH".to_string(),
        token_address: None,
        tx_hash: format!("{:#x}", tx_hash),
        block_number: Some(12345678),
        detected_at: Utc::now(),
        confirmed_at: None,
        from_address: Some("0xabababababababababababababababababababab".to_string()),
        reorged: false,
        extra: None,
        credited_amount: Some("1".to_string()),
        rate_used: None,
        rate_applied_at: None,
    };
    PaymentWriter::upsert(&*ds, &payment).await.unwrap();

    let event = PaymentConfirmed {
        tx_index: 0,
        chain_id: 1,
        invoice_id: uuid::Uuid::parse_str(invoice_id.as_str()).unwrap(),
        payment_address: Address::ZERO,
        amount: U256::from(1000000000000000000u64),
        tx_hash,
        block_number: 12345678,
        confirmations: 12,
        confirmed_at: Utc::now(),
    };

    consumer
        .handle_payment_confirmed(event.clone())
        .await
        .unwrap();
    assert_eq!(
        sink.jobs().len(),
        1,
        "the first apply must queue exactly one webhook"
    );

    // Redeliver the identical event - exactly what a resumed consumer would
    // hand `apply_envelope` again if the durable cursor never advanced past
    // it.
    consumer.handle_payment_confirmed(event).await.unwrap();
    assert_eq!(
        sink.jobs().len(),
        1,
        "redelivering an already-confirmed payment must not queue a second webhook"
    );
}

/// 2^96 base units - one past the largest integer `rust_decimal::Decimal`
/// (96-bit mantissa) can represent exactly. A payment for exactly the
/// invoice's expected amount at this magnitude must still be recognized as
/// fully paid: the comparison that decides paid/underpaid/overpaid must not
/// round or fail just because the number is large.
#[tokio::test]
async fn test_handle_payment_confirmed_exact_amount_beyond_decimal_precision() {
    let ds = Arc::new(InMemoryDataService::new());
    let bridge = Arc::new(MemoryBridge::new());
    let consumer = create_test_consumer(ds.clone(), bridge.clone());

    let invoice_id = InvoiceId::new();
    let store_id = StoreId::new();

    let exact_amount = "79228162514264337593543950336"; // 2^96

    let invoice = InvoiceData {
        id: invoice_id.clone(),
        store_id,
        currency: "ETH".to_string(),
        status: InvoiceStatus::Processing,
        amount: exact_amount.to_string(),
        amount_received: exact_amount.to_string(),
        created_at: Utc::now(),
        expires_at: Utc::now() + chrono::Duration::hours(1),
        metadata: None,
        customer_email: None,
        extra: None,
    };
    InvoiceWriter::upsert(&*ds, &invoice).await.unwrap();

    let tx_hash = B256::repeat_byte(0x11);
    let payment = PaymentData {
        id: Uuid::new_v4(),
        invoice_id: invoice_id.clone(),
        payment_option_id: None,
        chain_id: ChainId::parse("eip155:1").unwrap(),
        asset_type: types::AssetType::Native,
        amount: exact_amount.to_string(),
        asset_symbol: "ETH".to_string(),
        token_address: None,
        tx_hash: format!("{:#x}", tx_hash),
        block_number: Some(1),
        detected_at: Utc::now(),
        confirmed_at: None,
        from_address: Some("0x1111111111111111111111111111111111111111".to_string()),
        reorged: false,
        extra: None,
        credited_amount: Some(exact_amount.to_string()),
        rate_used: None,
        rate_applied_at: None,
    };
    PaymentWriter::upsert(&*ds, &payment).await.unwrap();

    let event = PaymentConfirmed {
        tx_index: 0,
        chain_id: 1,
        invoice_id: uuid::Uuid::parse_str(invoice_id.as_str()).unwrap(),
        payment_address: Address::ZERO,
        amount: U256::from_str_radix(exact_amount, 10).unwrap(),
        tx_hash,
        block_number: 1,
        confirmations: 12,
        confirmed_at: Utc::now(),
    };

    consumer.handle_payment_confirmed(event).await.unwrap();

    let invoice = InvoiceReader::get(&*ds, &invoice_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        invoice.status,
        InvoiceStatus::Paid,
        "a payment matching the invoice exactly at 2^96 base units must be marked paid"
    );
}

/// One base unit short of the exact amount in
/// `test_handle_payment_confirmed_exact_amount_beyond_decimal_precision`. A
/// fix that rounds everything at this magnitude up to "paid" would pass that
/// test alone; this catches it by requiring the shortfall to still read as
/// unpaid.
#[tokio::test]
async fn test_handle_payment_confirmed_one_unit_below_exact_amount_stays_unpaid() {
    let ds = Arc::new(InMemoryDataService::new());
    let bridge = Arc::new(MemoryBridge::new());
    let consumer = create_test_consumer(ds.clone(), bridge.clone());

    let invoice_id = InvoiceId::new();
    let store_id = StoreId::new();

    let expected_amount = "79228162514264337593543950336"; // 2^96
    let received_amount = "79228162514264337593543950335"; // one base unit short

    // A store that accepts no shortfall at all. Under the default tolerance a
    // one-unit gap at this magnitude is dust and would settle; this test is
    // about the comparison itself not rounding, so it takes the tolerance out.
    data_service::SettlementToleranceWriter::set_settlement_tolerance(&*ds, store_id.0, "0")
        .await
        .unwrap();

    let invoice = InvoiceData {
        id: invoice_id.clone(),
        store_id,
        currency: "ETH".to_string(),
        status: InvoiceStatus::Processing,
        amount: expected_amount.to_string(),
        amount_received: received_amount.to_string(),
        created_at: Utc::now(),
        expires_at: Utc::now() + chrono::Duration::hours(1),
        metadata: None,
        customer_email: None,
        extra: None,
    };
    InvoiceWriter::upsert(&*ds, &invoice).await.unwrap();

    let tx_hash = B256::repeat_byte(0x22);
    let payment = PaymentData {
        id: Uuid::new_v4(),
        invoice_id: invoice_id.clone(),
        payment_option_id: None,
        chain_id: ChainId::parse("eip155:1").unwrap(),
        asset_type: types::AssetType::Native,
        amount: received_amount.to_string(),
        asset_symbol: "ETH".to_string(),
        token_address: None,
        tx_hash: format!("{:#x}", tx_hash),
        block_number: Some(1),
        detected_at: Utc::now(),
        confirmed_at: None,
        from_address: Some("0x2222222222222222222222222222222222222222".to_string()),
        reorged: false,
        extra: None,
        credited_amount: Some(received_amount.to_string()),
        rate_used: None,
        rate_applied_at: None,
    };
    PaymentWriter::upsert(&*ds, &payment).await.unwrap();

    let event = PaymentConfirmed {
        tx_index: 0,
        chain_id: 1,
        invoice_id: uuid::Uuid::parse_str(invoice_id.as_str()).unwrap(),
        payment_address: Address::ZERO,
        amount: U256::from_str_radix(received_amount, 10).unwrap(),
        tx_hash,
        block_number: 1,
        confirmations: 12,
        confirmed_at: Utc::now(),
    };

    consumer.handle_payment_confirmed(event).await.unwrap();

    let invoice = InvoiceReader::get(&*ds, &invoice_id)
        .await
        .unwrap()
        .unwrap();
    // There is no `Underpaid` status to assert against - `InvoiceStatus` has
    // no such variant, since the handler simply leaves a not-fully-paid
    // invoice's status untouched. Paired with the exact-match test above
    // (which does verify a transition to `Paid` happens), asserting the
    // status is still exactly the pre-event `Processing` rules out both a
    // wrongly-early `Paid` transition and a handler that transitions
    // nothing at all.
    assert_eq!(
        invoice.status,
        InvoiceStatus::Processing,
        "one base unit short of the invoice amount must not be marked paid"
    );
}

#[tokio::test]
async fn test_handle_payment_confirmed_skips_cancelled_invoice() {
    let ds = Arc::new(InMemoryDataService::new());
    let bridge = Arc::new(MemoryBridge::new());
    let consumer = create_test_consumer(ds.clone(), bridge.clone());

    let invoice_id = InvoiceId::new();
    let store_id = StoreId::new();

    // Create cancelled invoice
    let invoice = InvoiceData {
        id: invoice_id.clone(),
        store_id,
        currency: "ETH".to_string(),
        status: InvoiceStatus::Cancelled,
        amount: "1000000000000000000".to_string(),
        amount_received: "1000000000000000000".to_string(),
        created_at: Utc::now(),
        expires_at: Utc::now() + chrono::Duration::hours(1),
        metadata: None,
        customer_email: None,
        extra: None,
    };
    InvoiceWriter::upsert(&*ds, &invoice).await.unwrap();

    // Create a payment record
    let tx_hash = B256::repeat_byte(0xab);
    let payment = PaymentData {
        id: Uuid::new_v4(),
        invoice_id: invoice_id.clone(),
        payment_option_id: None,
        chain_id: ChainId::parse("eip155:1").unwrap(),
        asset_type: types::AssetType::Native,
        amount: "1000000000000000000".to_string(),
        asset_symbol: "ETH".to_string(),
        token_address: None,
        tx_hash: format!("{:#x}", tx_hash),
        block_number: Some(12345678),
        detected_at: Utc::now(),
        confirmed_at: None,
        from_address: None,
        reorged: false,
        extra: None,
        credited_amount: Some("1".to_string()),
        rate_used: None,
        rate_applied_at: None,
    };
    PaymentWriter::upsert(&*ds, &payment).await.unwrap();

    // Create PaymentConfirmed event
    let event = PaymentConfirmed {
        tx_index: 0,
        chain_id: 1,
        invoice_id: uuid::Uuid::parse_str(invoice_id.as_str()).unwrap(),
        payment_address: Address::ZERO,
        amount: U256::from(1000000000000000000u64),
        tx_hash,
        block_number: 12345678,
        confirmations: 12,
        confirmed_at: Utc::now(),
    };

    // Handle the event
    consumer.handle_payment_confirmed(event).await.unwrap();

    // Invoice should still be cancelled
    let invoice = InvoiceReader::get(&*ds, &invoice_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(invoice.status, InvoiceStatus::Cancelled);
}

#[tokio::test]
async fn test_handle_payment_confirmed_late_payment_on_expired_invoice() {
    let ds = Arc::new(InMemoryDataService::new());
    let bridge = Arc::new(MemoryBridge::new());
    let consumer = create_test_consumer(ds.clone(), bridge.clone());

    let invoice_id = InvoiceId::new();
    let store_id = StoreId::new();

    // Create an expired invoice (with full amount received - late payment scenario)
    let invoice = InvoiceData {
        id: invoice_id.clone(),
        store_id,
        currency: "ETH".to_string(),
        status: InvoiceStatus::Expired,
        amount: "1000000000000000000".to_string(), // 1 ETH
        amount_received: "1000000000000000000".to_string(), // Full amount received after expiry
        created_at: Utc::now() - chrono::Duration::hours(2),
        expires_at: Utc::now() - chrono::Duration::hours(1), // Expired an hour ago
        metadata: None,
        customer_email: None,
        extra: None,
    };
    InvoiceWriter::upsert(&*ds, &invoice).await.unwrap();

    // Create a payment record (detected after expiry)
    let tx_hash = B256::repeat_byte(0xcc);
    let payment = PaymentData {
        id: Uuid::new_v4(),
        invoice_id: invoice_id.clone(),
        payment_option_id: None,
        chain_id: ChainId::parse("eip155:1").unwrap(),
        asset_type: types::AssetType::Native,
        amount: "1000000000000000000".to_string(),
        asset_symbol: "ETH".to_string(),
        token_address: None,
        tx_hash: format!("{:#x}", tx_hash),
        block_number: Some(12345700),
        detected_at: Utc::now(),
        confirmed_at: None,
        from_address: Some("0xcccccccccccccccccccccccccccccccccccccccc".to_string()),
        reorged: false,
        extra: None,
        credited_amount: Some("1".to_string()),
        rate_used: None,
        rate_applied_at: None,
    };
    PaymentWriter::upsert(&*ds, &payment).await.unwrap();

    // Create PaymentConfirmed event for the late payment
    let event = PaymentConfirmed {
        tx_index: 0,
        chain_id: 1,
        invoice_id: uuid::Uuid::parse_str(invoice_id.as_str()).unwrap(),
        payment_address: Address::ZERO,
        amount: U256::from(1000000000000000000u64),
        tx_hash,
        block_number: 12345700,
        confirmations: 12,
        confirmed_at: Utc::now(),
    };

    // Handle the event
    consumer.handle_payment_confirmed(event).await.unwrap();

    // Verify invoice was marked as LatePaid (not Paid)
    let invoice = InvoiceReader::get(&*ds, &invoice_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(invoice.status, InvoiceStatus::LatePaid);

    // Verify payment was marked as confirmed
    let payments = PaymentReader::get_for_invoice(&*ds, &invoice_id)
        .await
        .unwrap();
    assert!(payments[0].confirmed_at.is_some());
}

#[tokio::test]
async fn test_receipt_sent_on_paid_with_email() {
    let ds = Arc::new(InMemoryDataService::new());
    let bridge = Arc::new(MemoryBridge::new());
    let mock_email = Arc::new(MockEmailSender::new());
    let consumer: EventConsumer<_, MockEVMMonitor> = EventConsumer::new(
        bridge.clone(),
        ds.clone(),
        None,
        None,
        None,
        mock_email.clone(),
    );

    let invoice_id = InvoiceId::new();
    let store_id = StoreId::new();

    // Legacy path: address inside metadata, no column. Covers invoices created
    // before `customer_email` became a column; new writes populate it instead.
    let invoice = InvoiceData {
        id: invoice_id.clone(),
        store_id,
        currency: "USD".to_string(),
        status: InvoiceStatus::Processing,
        amount: "100.00".to_string(),
        amount_received: "100.00".to_string(),
        created_at: Utc::now(),
        expires_at: Utc::now() + chrono::Duration::hours(1),
        metadata: Some(serde_json::json!({"customer_email": "buyer@example.com"})),
        customer_email: None,
        extra: None,
    };
    InvoiceWriter::upsert(&*ds, &invoice).await.unwrap();

    // Create payment
    let tx_hash = B256::repeat_byte(0xee);
    let payment = PaymentData {
        id: Uuid::new_v4(),
        invoice_id: invoice_id.clone(),
        payment_option_id: None,
        chain_id: ChainId::parse("eip155:1").unwrap(),
        asset_type: types::AssetType::Native,
        amount: "50000000000000000".to_string(),
        asset_symbol: "ETH".to_string(),
        token_address: None,
        tx_hash: format!("{:#x}", tx_hash),
        block_number: Some(12346000),
        detected_at: Utc::now(),
        confirmed_at: None,
        from_address: Some("0xeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee".to_string()),
        reorged: false,
        extra: None,
        credited_amount: Some("100.00".to_string()),
        rate_used: Some("2000.00".to_string()),
        rate_applied_at: Some(Utc::now()),
    };
    PaymentWriter::upsert(&*ds, &payment).await.unwrap();

    // Handle payment confirmed event
    let event = PaymentConfirmed {
        tx_index: 0,
        chain_id: 1,
        invoice_id: uuid::Uuid::parse_str(invoice_id.as_str()).unwrap(),
        payment_address: Address::ZERO,
        amount: U256::from(50000000000000000u64),
        tx_hash,
        block_number: 12346000,
        confirmations: 12,
        confirmed_at: Utc::now(),
    };

    consumer.handle_payment_confirmed(event).await.unwrap();

    // Verify receipt email was sent
    assert_eq!(mock_email.call_count(), 1);
    let calls = mock_email.calls();
    assert_eq!(calls[0].0, "buyer@example.com");
    assert_eq!(calls[0].1, invoice_id.as_str());
}

#[tokio::test]
async fn test_no_receipt_when_email_absent() {
    let ds = Arc::new(InMemoryDataService::new());
    let bridge = Arc::new(MemoryBridge::new());
    let mock_email = Arc::new(MockEmailSender::new());
    let consumer: EventConsumer<_, MockEVMMonitor> = EventConsumer::new(
        bridge.clone(),
        ds.clone(),
        None,
        None,
        None,
        mock_email.clone(),
    );

    let invoice_id = InvoiceId::new();
    let store_id = StoreId::new();

    // Create invoice WITHOUT customer_email
    let invoice = InvoiceData {
        id: invoice_id.clone(),
        store_id,
        currency: "USD".to_string(),
        status: InvoiceStatus::Processing,
        amount: "100.00".to_string(),
        amount_received: "100.00".to_string(),
        created_at: Utc::now(),
        expires_at: Utc::now() + chrono::Duration::hours(1),
        metadata: None,
        customer_email: None,
        extra: None,
    };
    InvoiceWriter::upsert(&*ds, &invoice).await.unwrap();

    // Create payment
    let tx_hash = B256::repeat_byte(0xff);
    let payment = PaymentData {
        id: Uuid::new_v4(),
        invoice_id: invoice_id.clone(),
        payment_option_id: None,
        chain_id: ChainId::parse("eip155:1").unwrap(),
        asset_type: types::AssetType::Native,
        amount: "50000000000000000".to_string(),
        asset_symbol: "ETH".to_string(),
        token_address: None,
        tx_hash: format!("{:#x}", tx_hash),
        block_number: Some(12347000),
        detected_at: Utc::now(),
        confirmed_at: None,
        from_address: Some("0xffffffffffffffffffffffffffffffffffffffff".to_string()),
        reorged: false,
        extra: None,
        credited_amount: Some("100.00".to_string()),
        rate_used: Some("2000.00".to_string()),
        rate_applied_at: Some(Utc::now()),
    };
    PaymentWriter::upsert(&*ds, &payment).await.unwrap();

    // Handle payment confirmed event
    let event = PaymentConfirmed {
        tx_index: 0,
        chain_id: 1,
        invoice_id: uuid::Uuid::parse_str(invoice_id.as_str()).unwrap(),
        payment_address: Address::ZERO,
        amount: U256::from(50000000000000000u64),
        tx_hash,
        block_number: 12347000,
        confirmations: 12,
        confirmed_at: Utc::now(),
    };

    consumer.handle_payment_confirmed(event).await.unwrap();

    // Verify NO receipt email was sent
    assert_eq!(mock_email.call_count(), 0);
}

/// Receipts must read the `customer_email` column, not just metadata.
///
/// This is the path every invoice now takes - the address is
/// written to its own column and deliberately kept out of `metadata`, which is
/// slated to become ciphertext. Before the column was threaded
/// through, `extract_customer_email` looked only at metadata, so this case
/// returned None and the receipt was silently skipped: a missing address is a
/// normal, unlogged outcome there, so nothing would have reported the breakage.
#[tokio::test]
async fn test_receipt_sent_from_customer_email_column() {
    let ds = Arc::new(InMemoryDataService::new());
    let bridge = Arc::new(MemoryBridge::new());
    let mock_email = Arc::new(MockEmailSender::new());
    let consumer: EventConsumer<_, MockEVMMonitor> = EventConsumer::new(
        bridge.clone(),
        ds.clone(),
        None,
        None,
        None,
        mock_email.clone(),
    );

    let invoice_id = InvoiceId::new();
    let store_id = StoreId::new();

    // Column set, metadata empty - the inverse of the legacy test above.
    let invoice = InvoiceData {
        id: invoice_id.clone(),
        store_id,
        currency: "USD".to_string(),
        status: InvoiceStatus::Processing,
        amount: "100.00".to_string(),
        amount_received: "100.00".to_string(),
        created_at: Utc::now(),
        expires_at: Utc::now() + chrono::Duration::hours(1),
        metadata: None,
        customer_email: Some("column@example.com".to_string()),
        extra: None,
    };
    InvoiceWriter::upsert(&*ds, &invoice).await.unwrap();

    let tx_hash = B256::repeat_byte(0xcc);
    let payment = PaymentData {
        id: Uuid::new_v4(),
        invoice_id: invoice_id.clone(),
        payment_option_id: None,
        chain_id: ChainId::parse("eip155:1").unwrap(),
        asset_type: types::AssetType::Native,
        amount: "50000000000000000".to_string(),
        asset_symbol: "ETH".to_string(),
        token_address: None,
        tx_hash: format!("{:#x}", tx_hash),
        block_number: Some(12347000),
        detected_at: Utc::now(),
        confirmed_at: None,
        from_address: Some("0xcccccccccccccccccccccccccccccccccccccccc".to_string()),
        reorged: false,
        extra: None,
        credited_amount: Some("100.00".to_string()),
        rate_used: Some("2000.00".to_string()),
        rate_applied_at: Some(Utc::now()),
    };
    PaymentWriter::upsert(&*ds, &payment).await.unwrap();

    consumer
        .handle_payment_confirmed(PaymentConfirmed {
            tx_index: 0,
            chain_id: 1,
            invoice_id: uuid::Uuid::parse_str(invoice_id.as_str()).unwrap(),
            payment_address: Address::ZERO,
            amount: U256::from(50000000000000000u64),
            tx_hash,
            block_number: 12347000,
            confirmations: 12,
            confirmed_at: Utc::now(),
        })
        .await
        .unwrap();

    assert_eq!(
        mock_email.call_count(),
        1,
        "receipt must be sent from the column"
    );
    assert_eq!(mock_email.calls()[0].0, "column@example.com");
}

/// Two transfers to the *same* invoice in one transaction must both be
/// confirmed.
///
/// A transaction can pay one invoice twice - a native transfer plus an ERC20
/// transfer, or two ERC20 transfers from a batching contract. Since the
/// payments table gained `tx_index` both rows are stored, but the handler
/// still resolved a confirmation with `find(|p| p.tx_hash == tx_hash)` over
/// the invoice's payments. That returns whichever row came first, so the
/// second confirmation re-confirmed the first row - `mark_confirmed` is a
/// no-op once set - and the second transfer stayed unconfirmed for good, with
/// no confirmed webhook and no receipt.
///
/// Scoping by invoice is not enough to disambiguate here, which is why this
/// case and not the two-invoice one is what pins the handler down.
#[tokio::test]
async fn two_transfers_to_one_invoice_in_one_transaction_are_both_confirmed() {
    let ds = Arc::new(InMemoryDataService::new());
    let bridge = Arc::new(MemoryBridge::new());
    let consumer = create_test_consumer(ds.clone(), bridge.clone());

    let invoice_id = InvoiceId::new();
    let store_id = StoreId::new();
    let tx_hash = B256::repeat_byte(0xcd);
    let chain = ChainId::parse("eip155:1").unwrap();

    let invoice = InvoiceData {
        id: invoice_id.clone(),
        store_id,
        amount: "2".to_string(),
        currency: "ETH".to_string(),
        amount_received: "2".to_string(),
        status: InvoiceStatus::Processing,
        created_at: Utc::now(),
        expires_at: Utc::now() + chrono::Duration::hours(1),
        customer_email: None,
        metadata: None,
        extra: None,
    };
    InvoiceWriter::upsert(&*ds, &invoice).await.unwrap();

    // Two transfers, one transaction, one invoice - distinguished only by the
    // log index within the transaction.
    let mut payment_ids = Vec::new();
    for tx_index in [0i32, 1i32] {
        let payment = PaymentData {
            id: Uuid::new_v4(),
            invoice_id: invoice_id.clone(),
            payment_option_id: None,
            chain_id: chain.clone(),
            asset_type: types::AssetType::ERC20,
            amount: "1000000000000000000".to_string(),
            asset_symbol: "ETH".to_string(),
            token_address: Some("0xdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef".to_string()),
            tx_hash: format!("{:#x}", tx_hash),
            block_number: Some(12_345_678),
            detected_at: Utc::now(),
            confirmed_at: None,
            from_address: Some("0xcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd".to_string()),
            reorged: false,
            extra: None,
            credited_amount: Some("1".to_string()),
            rate_used: None,
            rate_applied_at: None,
        };
        payment_ids.push(payment.id);
        // Written the way the monitor writes them: keyed by the transfer.
        data_service::PaymentTxIndexWriter::upsert_with_tx_index(&*ds, &payment, tx_index)
            .await
            .unwrap();
    }

    for tx_index in [0i32, 1i32] {
        consumer
            .handle_payment_confirmed(PaymentConfirmed {
                tx_index,
                chain_id: 1,
                invoice_id: Uuid::parse_str(invoice_id.as_str()).unwrap(),
                payment_address: Address::ZERO,
                amount: U256::from(1_000_000_000_000_000_000u64),
                tx_hash,
                block_number: 12_345_678,
                confirmations: 12,
                confirmed_at: Utc::now(),
            })
            .await
            .unwrap();
    }

    let payments = PaymentReader::get_for_invoice(&*ds, &invoice_id)
        .await
        .unwrap();
    let confirmed = payments.iter().filter(|p| p.confirmed_at.is_some()).count();
    assert_eq!(
        confirmed, 2,
        "both transfers in the transaction must be confirmed; matching on tx_hash \
         alone re-confirms the first row and leaves the second unconfirmed forever"
    );
}
