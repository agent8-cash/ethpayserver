#![allow(clippy::unwrap_used, clippy::expect_used)]

use async_trait::async_trait;
use chrono::Utc;
use data_service::InMemoryDataService;
use evm::{Address, U256};
use std::sync::Arc;
use types::{ChainId, InvoiceData, InvoiceId, InvoiceStatus, InvoiceWriter, StoreId};

use crate::services::email;
use crate::services::evm_monitor::{EVMMonitor, EVMMonitorError};
use crate::services::webhook::{WebhookError, WebhookJob, WebhookSink};

/// Native asset symbol for a chain (test helper).
///
/// Keyed on the CAIP-2 identifier. In production this comes from
/// `chain_configs`; the tests keep a small table so they do not need a database
/// row to assert on a symbol.
pub fn native_symbol(chain_id: &ChainId) -> String {
    match chain_id.as_str() {
        "eip155:1" | "eip155:42161" | "eip155:10" | "eip155:8453" | "eip155:324"
        | "eip155:59144" | "eip155:534352" => "ETH",
        "eip155:137" => "POL",
        "eip155:43114" => "AVAX",
        "eip155:56" => "BNB",
        "eip155:250" => "FTM",
        "eip155:100" => "xDAI",
        _ => "UNKNOWN",
    }
    .to_string()
}

/// Mock EVMMonitor for testing.
pub struct MockEVMMonitor;

#[async_trait]
impl EVMMonitor for MockEVMMonitor {
    async fn watch_address(
        &self,
        _chain_id: &ChainId,
        _address: Address,
        _invoice_id: uuid::Uuid,
        _expected_amount: Option<U256>,
        _token_contract: Option<Address>,
    ) -> Result<(), EVMMonitorError> {
        Ok(())
    }

    async fn watch_address_by_chain_id(
        &self,
        _chain_id: u64,
        _address: Address,
        _invoice_id: uuid::Uuid,
        _expected_amount: Option<U256>,
        _token_contract: Option<Address>,
    ) -> Result<(), EVMMonitorError> {
        Ok(())
    }

    async fn unwatch_address(
        &self,
        _chain_id: &ChainId,
        _address: Address,
        _token_contract: Option<Address>,
    ) -> Result<(), EVMMonitorError> {
        Ok(())
    }

    async fn unwatch_address_by_chain_id(
        &self,
        _chain_id: u64,
        _address: Address,
        _token_contract: Option<Address>,
    ) -> Result<(), EVMMonitorError> {
        Ok(())
    }

    async fn health_check(&self) -> Result<(), EVMMonitorError> {
        Ok(())
    }

    async fn get_chain_health(&self) -> Result<Vec<evm::monitor::ChainHealth>, EVMMonitorError> {
        Ok(vec![])
    }
}

/// Create a test invoice in the data service.
pub async fn create_test_invoice(
    ds: &InMemoryDataService,
    invoice_id: &InvoiceId,
    store_id: StoreId,
) {
    let invoice = InvoiceData {
        id: invoice_id.clone(),
        store_id,
        currency: "ETH".to_string(),
        status: InvoiceStatus::Pending,
        amount: "1000000000000000000".to_string(), // 1 ETH
        amount_received: "0".to_string(),
        created_at: Utc::now(),
        expires_at: Utc::now() + chrono::Duration::hours(1),
        metadata: None,
        customer_email: None,
        extra: None,
    };
    InvoiceWriter::upsert(ds, &invoice).await.unwrap();
}

/// Mock email sender that records calls for test assertions.
pub struct MockEmailSender {
    calls: std::sync::Mutex<Vec<(String, String)>>, // (to, invoice_id)
}

impl MockEmailSender {
    pub fn new() -> Self {
        Self {
            calls: std::sync::Mutex::new(Vec::new()),
        }
    }

    pub fn call_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }

    pub fn calls(&self) -> std::sync::MutexGuard<'_, Vec<(String, String)>> {
        self.calls.lock().unwrap()
    }
}

#[async_trait]
impl email::EmailSender for MockEmailSender {
    async fn send_receipt(
        &self,
        to: &str,
        data: &email::ReceiptData,
    ) -> Result<(), email::EmailError> {
        self.calls
            .lock()
            .unwrap()
            .push((to.to_string(), data.invoice_id.clone()));
        Ok(())
    }

    async fn send_email_change_verification(
        &self,
        _to: &str,
        _data: &email::EmailChangeVerificationData,
    ) -> Result<(), email::EmailError> {
        Ok(())
    }

    async fn send_account_notice(
        &self,
        _to: &str,
        _notice: &email::AccountNotice,
    ) -> Result<(), email::EmailError> {
        Ok(())
    }

    fn is_configured(&self) -> bool {
        true
    }
}

/// Create a consumer with in-memory data service and no-op email.
pub fn create_test_consumer(
    ds: Arc<InMemoryDataService>,
    bridge: Arc<evm::monitor::bridge::MemoryBridge>,
) -> super::super::EventConsumer<InMemoryDataService, MockEVMMonitor> {
    create_test_consumer_with_bridge(ds, bridge)
}

/// Records every cursor `subscribe_from` is called with, then delegates to
/// the real bridge underneath.
///
/// Every other assertion available to a consumer test only sees the
/// *outcome* of a resume - which envelopes end up applied - and an
/// unbounded [`evm::monitor::bridge::MemoryBridge`] plus idempotent apply
/// make "resumed from the persisted cursor" and "resumed from scratch and
/// relied on dedup" produce the exact same outcome. Wrapping the bridge to
/// record the actual argument is the only way to tell those two apart, so a
/// future change that silently dropped the resume cursor (e.g. hardcoding
/// `subscribe_from(None)`) has something in this suite that goes red for it
/// specifically.
pub struct RecordingBridge {
    inner: Arc<dyn evm::monitor::bridge::EventBridge>,
    subscribe_from_calls: std::sync::Mutex<Vec<Option<evm::monitor::bridge::EventCursor>>>,
}

impl RecordingBridge {
    pub fn new(inner: Arc<dyn evm::monitor::bridge::EventBridge>) -> Self {
        Self {
            inner,
            subscribe_from_calls: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Every cursor passed to `subscribe_from`, in call order.
    pub fn subscribe_from_calls(&self) -> Vec<Option<evm::monitor::bridge::EventCursor>> {
        self.subscribe_from_calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl evm::monitor::bridge::EventBridge for RecordingBridge {
    async fn publish(&self, event: &evm::monitor::events::MonitorEvent) -> evm::EvmResult<()> {
        self.inner.publish(event).await
    }

    async fn subscribe_from(
        &self,
        from: Option<evm::monitor::bridge::EventCursor>,
    ) -> evm::EvmResult<evm::monitor::bridge::DurableEventStream> {
        self.subscribe_from_calls.lock().unwrap().push(from);
        self.inner.subscribe_from(from).await
    }

    async fn current_epoch(&self) -> evm::EvmResult<i64> {
        self.inner.current_epoch().await
    }

    async fn bump_epoch(&self) -> evm::EvmResult<i64> {
        self.inner.bump_epoch().await
    }

    async fn publish_command(
        &self,
        command: &evm::monitor::events::MonitorCommand,
    ) -> evm::EvmResult<()> {
        self.inner.publish_command(command).await
    }

    async fn subscribe_commands(&self) -> evm::EvmResult<evm::monitor::bridge::CommandStream> {
        self.inner.subscribe_commands().await
    }

    fn name(&self) -> &str {
        self.inner.name()
    }

    async fn health_check(&self) -> evm::EvmResult<()> {
        self.inner.health_check().await
    }
}

/// Create a consumer against any [`evm::monitor::bridge::EventBridge`],
/// rather than only the concrete [`evm::monitor::bridge::MemoryBridge`] -
/// needed to hand it a [`RecordingBridge`] instead.
pub fn create_test_consumer_with_bridge(
    ds: Arc<InMemoryDataService>,
    bridge: Arc<dyn evm::monitor::bridge::EventBridge>,
) -> super::super::EventConsumer<InMemoryDataService, MockEVMMonitor> {
    super::super::EventConsumer::new(
        bridge,
        ds,
        None,
        None,
        None,
        Arc::new(email::NoopEmailSender),
    )
}

/// A capability-4 observer that records instead of calling a plugin.
///
/// The production path runs a wasm call; before this existed nothing could
/// see whether `handle_payment_confirmed` dispatched at all - which is the
/// failure mode the module's own unit tests cannot catch, since they call the
/// dispatcher directly rather than reaching it through the handler.
#[derive(Default)]
pub struct RecordingPaymentObserver {
    settled: std::sync::Mutex<Vec<crate::services::plugins::OwnStorePayment>>,
}

impl RecordingPaymentObserver {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn settled(&self) -> Vec<crate::services::plugins::OwnStorePayment> {
        self.settled.lock().unwrap().clone()
    }
}

#[async_trait]
impl crate::services::plugins::OwnStorePaymentObserver for RecordingPaymentObserver {
    async fn payment_settled(&self, payment: &crate::services::plugins::OwnStorePayment) {
        self.settled.lock().unwrap().push(payment.clone());
    }
}

/// A consumer that reports settled invoices on `own_store` to `observer`.
pub fn create_test_consumer_with_observer(
    ds: Arc<InMemoryDataService>,
    bridge: Arc<evm::monitor::bridge::MemoryBridge>,
    own_store: StoreId,
    observer: Arc<RecordingPaymentObserver>,
) -> super::super::EventConsumer<InMemoryDataService, MockEVMMonitor> {
    create_test_consumer(ds, bridge).with_own_store_payments(
        own_store,
        vec![observer as Arc<dyn crate::services::plugins::OwnStorePaymentObserver>],
    )
}

/// A webhook queue that records instead of delivering.
///
/// The production sink needs Redis, so before this existed no test could see
/// whether a handler emitted an event at all - which is how `payment_reorged`
/// came to be missing without anything failing.
#[derive(Default)]
pub struct RecordingWebhookSink {
    jobs: std::sync::Mutex<Vec<WebhookJob>>,
}

impl RecordingWebhookSink {
    pub fn new() -> Self {
        Self::default()
    }

    /// Every job queued so far, in order.
    pub fn jobs(&self) -> Vec<WebhookJob> {
        self.jobs.lock().unwrap().clone()
    }
}

#[async_trait]
impl WebhookSink for RecordingWebhookSink {
    async fn queue(&self, job: WebhookJob) -> Result<(), WebhookError> {
        self.jobs.lock().unwrap().push(job);
        Ok(())
    }
}

/// Create a consumer whose webhook emissions are recorded rather than queued.
pub fn create_test_consumer_with_webhook(
    ds: Arc<InMemoryDataService>,
    bridge: Arc<evm::monitor::bridge::MemoryBridge>,
    sink: Arc<RecordingWebhookSink>,
) -> super::super::EventConsumer<InMemoryDataService, MockEVMMonitor> {
    super::super::EventConsumer::new(
        bridge,
        ds,
        None,
        Some(sink as Arc<dyn WebhookSink>),
        None,
        Arc::new(email::NoopEmailSender),
    )
}
