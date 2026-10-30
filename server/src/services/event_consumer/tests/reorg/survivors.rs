use super::*;

/// A payment that has already confirmed — and so would have been dropped
/// from the monitor's in-memory pending-payment map — must still be
/// retracted by a reorg at or below its block. `affected_invoices` is left
/// empty on purpose, to prove the candidate set comes from the database and
/// not from that field.
#[tokio::test]
async fn test_reorg_retracts_an_already_confirmed_payment() {
    let ds = Arc::new(InMemoryDataService::new());
    let bridge = Arc::new(MemoryBridge::new());
    let consumer = create_test_consumer(ds.clone(), bridge.clone());

    let invoice_id = InvoiceId::new();
    let store_id = StoreId::new();
    InvoiceWriter::upsert(&*ds, &fully_paid_invoice(&invoice_id, store_id))
        .await
        .unwrap();

    let mut payment = reorgable_payment(&invoice_id, "0xconfirmed", 100);
    payment.confirmed_at = Some(Utc::now());
    PaymentWriter::upsert(&*ds, &payment).await.unwrap();

    consumer
        .handle_reorg_detected(ReorgDetected {
            survivors_verifiable: true,
            affected_invoices: vec![],
            ..reorg_at(&invoice_id, 99)
        })
        .await
        .unwrap();

    let payments = PaymentReader::get_for_invoice(&*ds, &invoice_id)
        .await
        .unwrap();
    assert!(
        payments[0].reorged,
        "a confirmed payment must still be retracted"
    );

    let invoice = InvoiceReader::get(&*ds, &invoice_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(invoice.status, InvoiceStatus::Pending);
}

/// A reorg arriving with an empty `affected_invoices` — standing in for one
/// that arrives after a monitor restart, when the in-memory pending-payment
/// map has nothing in it — must still find and retract the affected
/// payments, because the candidate set is read from the database.
#[tokio::test]
async fn test_reorg_after_restart_still_finds_affected_payments() {
    let ds = Arc::new(InMemoryDataService::new());
    let bridge = Arc::new(MemoryBridge::new());
    let consumer = create_test_consumer(ds.clone(), bridge.clone());

    let invoice_id = InvoiceId::new();
    let store_id = StoreId::new();
    InvoiceWriter::upsert(&*ds, &fully_paid_invoice(&invoice_id, store_id))
        .await
        .unwrap();
    PaymentWriter::upsert(&*ds, &reorgable_payment(&invoice_id, "0xunconfirmed", 100))
        .await
        .unwrap();

    consumer
        .handle_reorg_detected(ReorgDetected {
            survivors_verifiable: true,
            affected_invoices: vec![],
            ..reorg_at(&invoice_id, 99)
        })
        .await
        .unwrap();

    let payments = PaymentReader::get_for_invoice(&*ds, &invoice_id)
        .await
        .unwrap();
    assert!(payments[0].reorged);
}

/// A transaction that survives the reorg in a different block must not be
/// retracted — the opposite error, which un-pays an invoice that is still
/// genuinely paid. This is the case a naive "retract everything at or above
/// the fork block" fix gets wrong.
#[tokio::test]
async fn test_reorg_does_not_retract_a_survived_transaction() {
    let ds = Arc::new(InMemoryDataService::new());
    let bridge = Arc::new(MemoryBridge::new());
    let consumer = create_test_consumer(ds.clone(), bridge.clone());

    let invoice_id = InvoiceId::new();
    let store_id = StoreId::new();
    InvoiceWriter::upsert(&*ds, &fully_paid_invoice(&invoice_id, store_id))
        .await
        .unwrap();

    let survivor_hash = B256::repeat_byte(0x42);
    // Hardcoded, not `format!("{:#x}", survivor_hash)`: that would just check
    // `tx_hash_eq` against itself. This is the literal string
    // `payment_handler.rs`'s `format!("{:#x}", event.tx_hash)` actually
    // writes to `payments.tx_hash`, matching what a real row looks like.
    let tx_hash = "0x4242424242424242424242424242424242424242424242424242424242424242";
    PaymentWriter::upsert(&*ds, &reorgable_payment(&invoice_id, tx_hash, 100))
        .await
        .unwrap();

    consumer
        .handle_reorg_detected(ReorgDetected {
            survivors_verifiable: true,
            survived_tx_hashes: vec![survivor_hash],
            ..reorg_at(&invoice_id, 99)
        })
        .await
        .unwrap();

    let payments = PaymentReader::get_for_invoice(&*ds, &invoice_id)
        .await
        .unwrap();
    assert!(
        !payments[0].reorged,
        "a transaction that survived elsewhere must not be retracted"
    );

    let invoice = InvoiceReader::get(&*ds, &invoice_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        invoice.status,
        InvoiceStatus::Processing,
        "an invoice with no retracted payments must not have its status touched"
    );
}

/// A reorg must not make a refunded invoice payable again.
///
/// Before confirmed payments were reorg candidates, this loop only ever saw
/// the monitor's `pending` map, which holds unconfirmed payments — so a
/// settled invoice was out of reach. Now that a confirmed payment can be
/// retracted, a reorg at or below its block reaches invoices the merchant has
/// already closed out, and `InvoiceWriter::update_status` is a bare
/// `UPDATE invoices SET status` that will happily walk one backwards.
///
/// The damage is concrete: a refunded invoice flipped to `Pending` is payable
/// again, and the refund the merchant already sent from their own wallet is
/// orphaned against it.
///
/// Goes red without the closed-status guard: the invoice comes back `Pending`.
#[tokio::test]
async fn a_reorg_does_not_reopen_a_refunded_invoice() {
    let ds = Arc::new(InMemoryDataService::new());
    let bridge = Arc::new(MemoryBridge::new());
    let consumer = create_test_consumer(ds.clone(), bridge.clone());

    let invoice_id = InvoiceId::new();
    let chain = ChainId::evm(1);

    let invoice = InvoiceData {
        id: invoice_id.clone(),
        store_id: StoreId::new(),
        amount: "1".to_string(),
        currency: "ETH".to_string(),
        amount_received: "1".to_string(),
        // The merchant has already refunded this from their own wallet.
        status: InvoiceStatus::Refunded,
        created_at: Utc::now(),
        expires_at: Utc::now() + chrono::Duration::hours(1),
        customer_email: None,
        metadata: None,
        extra: None,
    };
    InvoiceWriter::upsert(&*ds, &invoice).await.unwrap();

    let payment = PaymentData {
        id: Uuid::new_v4(),
        invoice_id: invoice_id.clone(),
        payment_option_id: None,
        chain_id: chain.clone(),
        asset_type: types::AssetType::Native,
        amount: "1000000000000000000".to_string(),
        asset_symbol: "ETH".to_string(),
        token_address: None,
        tx_hash: format!("{:#x}", B256::repeat_byte(0x7a)),
        block_number: Some(500),
        detected_at: Utc::now(),
        confirmed_at: Some(Utc::now()),
        from_address: Some("0x7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a".to_string()),
        reorged: false,
        extra: None,
        credited_amount: Some("1".to_string()),
        rate_used: None,
        rate_applied_at: None,
    };
    PaymentWriter::upsert(&*ds, &payment).await.unwrap();

    consumer
        .handle_reorg_detected(ReorgDetected {
            survivors_verifiable: true,
            chain_id: 1,
            fork_block: 499,
            old_hash: B256::repeat_byte(0x01),
            new_hash: B256::repeat_byte(0x02),
            depth: 2,
            // Nothing survived the re-scan: the payment really is gone from
            // the canonical chain.
            survived_tx_hashes: vec![],
            affected_invoices: vec![Uuid::parse_str(invoice_id.as_str()).unwrap()],
            detected_at: Utc::now(),
        })
        .await
        .unwrap();

    let after = InvoiceReader::get(&*ds, &invoice_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        after.status,
        InvoiceStatus::Refunded,
        "a refunded invoice must stay refunded; reopening it makes it payable \
         again and orphans the refund the merchant already sent"
    );

    // The retraction itself is still recorded - the chain really did drop it.
    let payments = PaymentReader::get_for_invoice(&*ds, &invoice_id)
        .await
        .unwrap();
    assert!(
        payments[0].reorged,
        "the payment must still be marked reorged; only the status transition is suppressed"
    );
}

/// A reorg whose survivor scan could not run at all (the monitor reports
/// `survivors_verifiable: false`, e.g. because the chain was unreachable
/// while re-validating) must retract nothing and notify nobody. An empty
/// `survived_tx_hashes` from a scan that never ran is indistinguishable from
/// one that ran and found every payment gone — so treating `survivors_verifiable:
/// false` like a normal empty survivor list would retract every payment at or
/// above the fork block on a mere RPC hiccup, not just a real chain drop.
///
/// Goes red without the `survivors_verifiable` check: the payment comes back
/// reorged and a retraction webhook fires for a reorg nothing actually verified.
#[tokio::test]
async fn test_reorg_with_unverifiable_survivors_retracts_nothing() {
    let ds = Arc::new(InMemoryDataService::new());
    let bridge = Arc::new(MemoryBridge::new());
    let sink = Arc::new(RecordingWebhookSink::new());
    let consumer = create_test_consumer_with_webhook(ds.clone(), bridge.clone(), sink.clone());

    let invoice_id = InvoiceId::new();
    let store_id = StoreId::new();
    ds.set_webhook(store_id.0, "https://example.com/hook", "secret");

    InvoiceWriter::upsert(&*ds, &fully_paid_invoice(&invoice_id, store_id))
        .await
        .unwrap();
    PaymentWriter::upsert(&*ds, &reorgable_payment(&invoice_id, "0xunverified", 100))
        .await
        .unwrap();

    consumer
        .handle_reorg_detected(ReorgDetected {
            survivors_verifiable: false,
            ..reorg_at(&invoice_id, 99)
        })
        .await
        .unwrap();

    let payments = PaymentReader::get_for_invoice(&*ds, &invoice_id)
        .await
        .unwrap();
    assert!(
        !payments[0].reorged,
        "an unverified reorg must not retract a payment it never scanned for"
    );

    let invoice = InvoiceReader::get(&*ds, &invoice_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        invoice.status,
        InvoiceStatus::Processing,
        "an invoice with no retracted payments must not have its status touched"
    );

    assert!(
        sink.jobs().is_empty(),
        "nothing was retracted, so a subscriber must not be told to undo anything"
    );
}
