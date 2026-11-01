#![allow(clippy::unwrap_used, clippy::expect_used)]

use async_trait::async_trait;
use chrono::Utc;
use data_service::InMemoryDataService;
use evm::{Address, U256};
use std::sync::Arc;
use types::{
    AssetType, ChainId, InvoiceData, InvoiceId, InvoiceStatus, InvoiceWriter, PaymentData,
    PaymentMethodId, PaymentOptionData, PaymentOptionId, PaymentOptionWriter, PaymentWriter,
    StoreId, WatchedAddressWriter,
};
use uuid::Uuid;

use super::super::evm_monitor::EVMMonitorError;
use super::*;
use crate::api::ws::{StatusUpdate, WsBroadcast};

/// Records every unwatch it is sent, so a test can assert exactly which
/// address did or didn't get cleaned up without a real evmmonitor.
#[derive(Default)]
struct RecordingEVMMonitor {
    unwatched: std::sync::Mutex<Vec<Address>>,
}

#[async_trait]
impl EVMMonitor for RecordingEVMMonitor {
    async fn watch_address(
        &self,
        _chain_id: &ChainId,
        _address: Address,
        _invoice_id: Uuid,
        _expected_amount: Option<U256>,
        _token_contract: Option<Address>,
    ) -> Result<(), EVMMonitorError> {
        Ok(())
    }

    async fn watch_address_by_chain_id(
        &self,
        _chain_id: u64,
        _address: Address,
        _invoice_id: Uuid,
        _expected_amount: Option<U256>,
        _token_contract: Option<Address>,
    ) -> Result<(), EVMMonitorError> {
        Ok(())
    }

    async fn unwatch_address(
        &self,
        _chain_id: &ChainId,
        address: Address,
        _token_contract: Option<Address>,
    ) -> Result<(), EVMMonitorError> {
        self.unwatched.lock().unwrap().push(address);
        Ok(())
    }

    async fn unwatch_address_by_chain_id(
        &self,
        _chain_id: u64,
        address: Address,
        _token_contract: Option<Address>,
    ) -> Result<(), EVMMonitorError> {
        self.unwatched.lock().unwrap().push(address);
        Ok(())
    }

    async fn health_check(&self) -> Result<(), EVMMonitorError> {
        Ok(())
    }

    async fn get_chain_health(&self) -> Result<Vec<evm::monitor::ChainHealth>, EVMMonitorError> {
        Ok(vec![])
    }
}

/// An `EVMMonitor` whose unwatch always fails - simulating Redis being
/// unreachable when cleanup tries to publish the unwatch command.
#[derive(Default)]
struct FailingUnwatchEVMMonitor;

#[async_trait]
impl EVMMonitor for FailingUnwatchEVMMonitor {
    async fn watch_address(
        &self,
        _chain_id: &ChainId,
        _address: Address,
        _invoice_id: Uuid,
        _expected_amount: Option<U256>,
        _token_contract: Option<Address>,
    ) -> Result<(), EVMMonitorError> {
        Ok(())
    }

    async fn watch_address_by_chain_id(
        &self,
        _chain_id: u64,
        _address: Address,
        _invoice_id: Uuid,
        _expected_amount: Option<U256>,
        _token_contract: Option<Address>,
    ) -> Result<(), EVMMonitorError> {
        Ok(())
    }

    async fn unwatch_address(
        &self,
        chain_id: &ChainId,
        _address: Address,
        _token_contract: Option<Address>,
    ) -> Result<(), EVMMonitorError> {
        Err(EVMMonitorError::NotAnEvmChain(chain_id.clone()))
    }

    async fn unwatch_address_by_chain_id(
        &self,
        _chain_id: u64,
        _address: Address,
        _token_contract: Option<Address>,
    ) -> Result<(), EVMMonitorError> {
        Err(EVMMonitorError::NotAnEvmChain(
            ChainId::parse("eip155:1").unwrap(),
        ))
    }

    async fn health_check(&self) -> Result<(), EVMMonitorError> {
        Ok(())
    }

    async fn get_chain_health(&self) -> Result<Vec<evm::monitor::ChainHealth>, EVMMonitorError> {
        Ok(vec![])
    }
}

/// Sets up a `Paid` invoice with one confirmed payment and one watched
/// address, `confirmed_at` set `age_secs` in the past. Returns the
/// service under test plus the address to assert on.
async fn paid_invoice_with_watched_address(
    chain_id: ChainId,
    paid_unwatch_grace_period_secs: u64,
    age_secs: i64,
) -> (
    InvoiceCleanupService<InMemoryDataService, RecordingEVMMonitor>,
    String,
) {
    let ds = Arc::new(InMemoryDataService::new());
    let address = "0x1111111111111111111111111111111111111111".to_string();

    let invoice_id = InvoiceId::new();
    let invoice = InvoiceData {
        id: invoice_id.clone(),
        store_id: StoreId::new(),
        currency: "ETH".to_string(),
        status: InvoiceStatus::Paid,
        amount: "1000000000000000000".to_string(),
        amount_received: "1000000000000000000".to_string(),
        created_at: Utc::now(),
        expires_at: Utc::now() + chrono::Duration::hours(1),
        metadata: None,
        customer_email: None,
        extra: None,
    };
    InvoiceWriter::upsert(&*ds, &invoice).await.unwrap();

    let payment_option_id = PaymentOptionId::new();
    let option = PaymentOptionData {
        id: payment_option_id.clone(),
        invoice_id: invoice_id.clone(),
        payment_method_id: PaymentMethodId::new("ETH", &chain_id),
        chain_id: chain_id.clone(),
        asset_symbol: "ETH".to_string(),
        token_address: None,
        decimals: 18,
        payment_address: address.clone(),
        wallet_id: None,
        derivation_index: None,
        amount: "1000000000000000000".to_string(),
        rate: None,
        rate_at: None,
        is_active: true,
        created_at: Utc::now(),
    };
    PaymentOptionWriter::create(&*ds, &option).await.unwrap();

    WatchedAddressWriter::upsert(&*ds, &address, &payment_option_id, &chain_id, None)
        .await
        .unwrap();

    let payment = PaymentData {
        id: Uuid::new_v4(),
        invoice_id: invoice_id.clone(),
        payment_option_id: Some(payment_option_id.0),
        chain_id: chain_id.clone(),
        asset_type: AssetType::Native,
        amount: "1000000000000000000".to_string(),
        asset_symbol: "ETH".to_string(),
        token_address: None,
        tx_hash: "0xabc123".to_string(),
        block_number: Some(100),
        detected_at: Utc::now(),
        confirmed_at: Some(Utc::now() - chrono::Duration::seconds(age_secs)),
        from_address: None,
        reorged: false,
        extra: None,
        credited_amount: Some("1.0".to_string()),
        rate_used: None,
        rate_applied_at: None,
    };
    PaymentWriter::upsert(&*ds, &payment).await.unwrap();

    let config = CleanupConfig {
        fallback_interval_secs: 60,
        unwatch_grace_period_secs: 60,
        paid_unwatch_grace_period_secs,
    };
    let service = InvoiceCleanupService::new(
        ds,
        Arc::new(RecordingEVMMonitor::default()),
        config,
        None,
        None,
    );

    (service, address)
}

/// A payment that confirmed moments ago must keep its address watched:
/// unwatching it immediately is exactly the gap a reviewer flagged — a
/// reorg arriving right after confirmation would find no
/// watched address to re-validate the relocated transaction against, and
/// retract a payment that is still genuinely on chain.
#[tokio::test]
async fn recently_confirmed_payment_keeps_its_address_watched() {
    let chain_id = ChainId::parse("eip155:1").unwrap();
    let (service, _address) = paid_invoice_with_watched_address(chain_id, 3600, 5).await;

    let stats = service.cleanup_addresses().await.unwrap();

    assert_eq!(stats.paid, 0, "address unwatched inside its grace period");
    assert!(service.evm_monitor.unwatched.lock().unwrap().is_empty());
}

/// A chain whose own confirmation depth and block time call for a longer
/// buffer than the operator's flat `paid_unwatch_grace_period_secs` must
/// still keep the address watched — the flat default is not itself a
/// per-chain reorg-depth assumption (Polygon needs 128 confirmations at
/// 2s/block; `min_paid_unwatch_grace_period_secs` floors the effective
/// grace period at twice that, 512s, well past the 60s configured here).
#[tokio::test]
async fn chain_with_deep_confirmations_gets_a_longer_floor_than_the_flat_default() {
    let chain_id = ChainId::parse("eip155:137").unwrap(); // Polygon
    let (service, _address) = paid_invoice_with_watched_address(chain_id, 60, 300).await;

    let stats = service.cleanup_addresses().await.unwrap();

    assert_eq!(
        stats.paid, 0,
        "300s is past the flat 60s default but inside Polygon's 512s floor"
    );
    assert!(service.evm_monitor.unwatched.lock().unwrap().is_empty());
}

/// Once the grace period has elapsed, the address is unwatched as before
/// — the mitigation narrows the reorg window, it does not keep every
/// paid address watched forever.
#[tokio::test]
async fn payment_confirmed_past_the_grace_period_gets_unwatched() {
    let chain_id = ChainId::parse("eip155:1").unwrap();
    let (service, address) = paid_invoice_with_watched_address(chain_id, 60, 3600).await;

    let stats = service.cleanup_addresses().await.unwrap();

    assert_eq!(stats.paid, 1);
    let unwatched = service.evm_monitor.unwatched.lock().unwrap();
    assert_eq!(unwatched.as_slice(), [address.parse::<Address>().unwrap()]);
}

/// A failed Redis unwatch must leave Postgres still listing the address
/// as active, so the next cleanup poll retries it - see
/// `unwatch_and_deactivate`'s ordering comment. Deactivating
/// unconditionally on a failed unwatch would turn a self-healing retry
/// into a permanently orphaned Redis watch with no path back to a clean
/// state.
#[tokio::test]
async fn a_failed_unwatch_leaves_the_address_active_for_retry() {
    let ds = Arc::new(InMemoryDataService::new());
    let chain_id = ChainId::parse("eip155:1").unwrap();
    let address = "0x3333333333333333333333333333333333333333".to_string();

    let invoice_id = InvoiceId::new();
    let invoice = InvoiceData {
        id: invoice_id.clone(),
        store_id: StoreId::new(),
        currency: "ETH".to_string(),
        status: InvoiceStatus::Cancelled,
        amount: "1000000000000000000".to_string(),
        amount_received: "0".to_string(),
        created_at: Utc::now(),
        expires_at: Utc::now() + chrono::Duration::hours(1),
        metadata: None,
        customer_email: None,
        extra: None,
    };
    InvoiceWriter::upsert(&*ds, &invoice).await.unwrap();

    let payment_option_id = PaymentOptionId::new();
    let option = PaymentOptionData {
        id: payment_option_id.clone(),
        invoice_id: invoice_id.clone(),
        payment_method_id: PaymentMethodId::new("ETH", &chain_id),
        chain_id: chain_id.clone(),
        asset_symbol: "ETH".to_string(),
        token_address: None,
        decimals: 18,
        payment_address: address.clone(),
        wallet_id: None,
        derivation_index: None,
        amount: "1000000000000000000".to_string(),
        rate: None,
        rate_at: None,
        is_active: true,
        created_at: Utc::now(),
    };
    PaymentOptionWriter::create(&*ds, &option).await.unwrap();

    WatchedAddressWriter::upsert(&*ds, &address, &payment_option_id, &chain_id, None)
        .await
        .unwrap();

    let config = CleanupConfig {
        fallback_interval_secs: 60,
        unwatch_grace_period_secs: 60,
        paid_unwatch_grace_period_secs: 3600,
    };
    let service: InvoiceCleanupService<InMemoryDataService, FailingUnwatchEVMMonitor> =
        InvoiceCleanupService::new(
            ds.clone(),
            Arc::new(FailingUnwatchEVMMonitor),
            config,
            None,
            None,
        );

    let stats = service.cleanup_addresses().await.unwrap();
    assert_eq!(
        stats.cancelled, 0,
        "the unwatch failed, so this was not counted as a successful cleanup"
    );

    let still_active = WatchedAddressReader::get_cancelled_for_cleanup(&*ds)
        .await
        .unwrap();
    assert_eq!(
        still_active.len(),
        1,
        "the failed unwatch must not be deactivated - it needs to be retried"
    );
}

/// An invoice that lapses with no payment ever seen must still tell its
/// store's listeners: the dashboard learns of expiry from this frame, not
/// by polling.
#[tokio::test]
async fn expiring_an_unpaid_invoice_publishes_a_frame_to_its_store() {
    let ds = Arc::new(InMemoryDataService::new());
    let store_id = StoreId::new();
    let invoice_id = InvoiceId::new();
    let invoice = InvoiceData {
        id: invoice_id.clone(),
        store_id,
        currency: "ETH".to_string(),
        status: InvoiceStatus::Pending,
        amount: "1".to_string(),
        amount_received: "0".to_string(),
        created_at: Utc::now() - chrono::Duration::seconds(10),
        expires_at: Utc::now() - chrono::Duration::seconds(5),
        metadata: None,
        customer_email: None,
        extra: None,
    };
    InvoiceWriter::upsert(&*ds, &invoice).await.unwrap();

    let ws = Arc::new(WsBroadcast::new(16));
    let mut rx = ws.subscribe_store(store_id);
    let service: InvoiceCleanupService<InMemoryDataService, RecordingEVMMonitor> =
        InvoiceCleanupService::new(
            ds,
            Arc::new(RecordingEVMMonitor::default()),
            CleanupConfig::default(),
            None,
            Some(ws),
        );

    assert_eq!(service.check_expired().await.unwrap(), 1);

    match rx.try_recv() {
        Ok(StatusUpdate::InvoiceStatus {
            invoice_id: id,
            status,
        }) => {
            assert_eq!(id, invoice_id.as_str());
            assert_eq!(status, "expired");
        }
        other => panic!("expected an expired frame, got {other:?}"),
    }
}
