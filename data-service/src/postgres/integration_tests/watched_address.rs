//! Watched address integration tests.

use types::{
    ChainId, InvoiceWriter, PaymentOptionWriter, WatchedAddressReader, WatchedAddressWriter,
};

use super::{create_test_service, seeded_test_invoice, test_payment_option, unique_address};

/// This is the query `hard_delete_store` and `delete_user_account` run before
/// deleting: unlike every other `WatchedAddressReader` query, it is not
/// scoped by invoice status, because the case that matters here is a
/// `pending`, never-expired invoice with an address still watched but no
/// payment recorded yet - exactly the one the status-scoped queries would
/// never surface.
#[tokio::test]
#[ignore]
async fn get_active_watched_addresses_for_stores_finds_a_still_pending_invoices_address() {
    let service = create_test_service().await;

    let invoice = seeded_test_invoice(&service).await;
    InvoiceWriter::upsert(&service, &invoice).await.unwrap();

    let payment_option = test_payment_option(&invoice.id, &ChainId::evm(1));
    PaymentOptionWriter::create(&service, &payment_option)
        .await
        .unwrap();

    let address = unique_address();
    WatchedAddressWriter::upsert(
        &service,
        &address,
        &payment_option.id,
        &ChainId::evm(1),
        None,
    )
    .await
    .unwrap();

    let found = service
        .get_active_watched_addresses_for_stores(&[invoice.store_id.0])
        .await
        .unwrap();

    assert_eq!(found.len(), 1);
    assert_eq!(found[0].address, address);
    assert_eq!(found[0].invoice_id, invoice.id.to_string());

    // Not returned for a store it does not belong to.
    let other_store = uuid::Uuid::new_v4();
    let found = service
        .get_active_watched_addresses_for_stores(&[other_store])
        .await
        .unwrap();
    assert!(found.is_empty());

    // Nor once deactivated - this is what the unwatch step does before the
    // store is actually deleted.
    WatchedAddressWriter::deactivate(&service, &address, &ChainId::evm(1), None)
        .await
        .unwrap();
    let found = service
        .get_active_watched_addresses_for_stores(&[invoice.store_id.0])
        .await
        .unwrap();
    assert!(found.is_empty());
}

#[tokio::test]
#[ignore]
async fn integration_watched_address_crud() {
    let service = create_test_service().await;

    // Create invoice first
    let invoice = seeded_test_invoice(&service).await;
    InvoiceWriter::upsert(&service, &invoice).await.unwrap();

    // Create payment option (watched_addresses link to payment_options now)
    let payment_option = test_payment_option(&invoice.id, &ChainId::evm(1));
    PaymentOptionWriter::create(&service, &payment_option)
        .await
        .unwrap();

    let address = unique_address();
    let chain_id = ChainId::evm(1);

    // Upsert watched address
    WatchedAddressWriter::upsert(&service, &address, &payment_option.id, &chain_id, None)
        .await
        .unwrap();

    // Get invoice_id by address
    let found = WatchedAddressReader::get_invoice_id(&service, &address, &chain_id, None)
        .await
        .unwrap();
    assert_eq!(found, Some(invoice.id.clone()));

    // Get payment_option_id by address
    let found_opt =
        WatchedAddressReader::get_payment_option_id(&service, &address, &chain_id, None)
            .await
            .unwrap();
    assert_eq!(found_opt, Some(payment_option.id.clone()));

    // Deactivate watched address
    WatchedAddressWriter::deactivate(&service, &address, &chain_id, None)
        .await
        .unwrap();

    // Should not find it anymore (deactivated)
    let found = WatchedAddressReader::get_invoice_id(&service, &address, &chain_id, None)
        .await
        .unwrap();
    assert!(found.is_none());
}

#[tokio::test]
#[ignore]
async fn integration_watched_address_different_chains() {
    let service = create_test_service().await;

    // Create invoice
    let invoice = seeded_test_invoice(&service).await;
    InvoiceWriter::upsert(&service, &invoice).await.unwrap();

    // Create payment options for different chains
    let po1 = test_payment_option(&invoice.id, &ChainId::evm(1));
    let po2 = test_payment_option(&invoice.id, &ChainId::evm(137));
    PaymentOptionWriter::create(&service, &po1).await.unwrap();
    PaymentOptionWriter::create(&service, &po2).await.unwrap();

    let address = unique_address();

    // Watch same address on different chain_ids
    WatchedAddressWriter::upsert(&service, &address, &po1.id, &ChainId::evm(1), None)
        .await
        .unwrap();
    WatchedAddressWriter::upsert(&service, &address, &po2.id, &ChainId::evm(137), None)
        .await
        .unwrap();

    // Each chain should return the correct payment option
    let found_eth =
        WatchedAddressReader::get_payment_option_id(&service, &address, &ChainId::evm(1), None)
            .await
            .unwrap();
    assert_eq!(found_eth, Some(po1.id.clone()));

    let found_polygon =
        WatchedAddressReader::get_payment_option_id(&service, &address, &ChainId::evm(137), None)
            .await
            .unwrap();
    assert_eq!(found_polygon, Some(po2.id.clone()));

    // Remove from one chain shouldn't affect the other
    WatchedAddressWriter::deactivate(&service, &address, &ChainId::evm(1), None)
        .await
        .unwrap();

    let found_eth =
        WatchedAddressReader::get_payment_option_id(&service, &address, &ChainId::evm(1), None)
            .await
            .unwrap();
    assert!(found_eth.is_none());

    let found_polygon =
        WatchedAddressReader::get_payment_option_id(&service, &address, &ChainId::evm(137), None)
            .await
            .unwrap();
    assert_eq!(found_polygon, Some(po2.id.clone()));
}

#[tokio::test]
#[ignore]
async fn integration_watched_address_upsert_replaces() {
    let service = create_test_service().await;

    // Create invoice
    let invoice = seeded_test_invoice(&service).await;
    InvoiceWriter::upsert(&service, &invoice).await.unwrap();

    // Two DISTINCT payment options: `unique_payment_option` is
    // (invoice_id, payment_method_id), and the method id is derived from the
    // chain, so both on chain 1 collide before the test can assert anything.
    let po1 = test_payment_option(&invoice.id, &ChainId::evm(1));
    let po2 = test_payment_option(&invoice.id, &ChainId::evm(137));
    PaymentOptionWriter::create(&service, &po1).await.unwrap();
    PaymentOptionWriter::create(&service, &po2).await.unwrap();

    let address = unique_address();
    let chain_id = ChainId::evm(1);

    // Watch address for payment_option1
    WatchedAddressWriter::upsert(&service, &address, &po1.id, &chain_id, None)
        .await
        .unwrap();

    let found = WatchedAddressReader::get_payment_option_id(&service, &address, &chain_id, None)
        .await
        .unwrap();
    assert_eq!(found, Some(po1.id.clone()));

    // Upsert same address for payment_option2 - should replace
    WatchedAddressWriter::upsert(&service, &address, &po2.id, &chain_id, None)
        .await
        .unwrap();

    let found = WatchedAddressReader::get_payment_option_id(&service, &address, &chain_id, None)
        .await
        .unwrap();
    assert_eq!(found, Some(po2.id.clone()));
}
