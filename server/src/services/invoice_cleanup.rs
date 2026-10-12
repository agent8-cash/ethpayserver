//! Invoice cleanup service.
//!
//! Handles invoice lifecycle cleanup:
//! - Expires pending invoices that have passed their expiration time
//! - Unwatches addresses for completed invoices (expired, paid, cancelled)
//! - Sends webhook notifications when invoices expire
//!
//! Triggered by:
//! - Block events from any chain (via EventConsumer)
//! - Periodic fallback timer

use std::pin::pin;
use std::sync::Arc;

use data_service::StoreWebhookReader;
use evm::Address;
use futures::StreamExt;
use types::{
    InvoiceId, InvoiceReader, InvoiceWriter, PaymentReader, StoreSettingsReader,
    WatchedAddressReader, WatchedAddressWriter,
};

use crate::api::ws::{StatusUpdate, WsBroadcast};
use crate::metrics;

use super::evm_monitor::EVMMonitor;
use super::webhook::{
    WebhookDataService, WebhookEventType, WebhookPayload, WebhookService, queue_for_store,
};

/// Trait alias for data service requirements.
///
/// A data service must implement all repository traits needed by the cleanup service.
pub trait CleanupDataService:
    InvoiceReader
    + InvoiceWriter
    + PaymentReader
    + WatchedAddressReader
    + WatchedAddressWriter
    + StoreWebhookReader
    + StoreSettingsReader
    + Send
    + Sync
{
}

/// Blanket implementation for any type implementing the required traits.
impl<T> CleanupDataService for T where
    T: InvoiceReader
        + InvoiceWriter
        + PaymentReader
        + WatchedAddressReader
        + WatchedAddressWriter
        + StoreWebhookReader
        + StoreSettingsReader
        + Send
        + Sync
{
}

/// Configuration for the invoice cleanup service.
#[derive(Debug, Clone)]
pub struct CleanupConfig {
    /// Fallback interval in seconds when no block events are received.
    pub fallback_interval_secs: u64,
    /// Grace period in seconds after invoice expires before unwatching address.
    /// This allows late payments to still be detected.
    pub unwatch_grace_period_secs: u64,
    /// Grace period in seconds after a payment confirms before unwatching its
    /// address. A reorg re-validates candidates by re-scanning currently
    /// watched addresses (see `find_survived_tx_hashes`), so an address
    /// unwatched too soon after confirmation makes a relocated-but-still-paid
    /// transaction indistinguishable from a genuinely gone one, and it gets
    /// retracted — the "opposite error", and the worse one. This keeps the
    /// address watched long enough to cover the realistic window in which a
    /// deep reorg would be detected and re-validated.
    ///
    /// This one value is a flat default across every chain, which is not by
    /// itself a per-chain reorg-depth assumption — `effective_paid_unwatch_grace_period_secs`
    /// raises it to `ChainConfig::min_paid_unwatch_grace_period_secs` for any
    /// chain whose own confirmation depth and block time call for more than
    /// this default gives it, rather than trusting one wall-clock number to
    /// fit every chain. It still does not make the window unbounded: a reorg
    /// arriving after the effective grace period has elapsed is a residual
    /// risk this service accepts, not one it closes.
    pub paid_unwatch_grace_period_secs: u64,
}

impl Default for CleanupConfig {
    fn default() -> Self {
        Self {
            fallback_interval_secs: 60,
            unwatch_grace_period_secs: 60,
            paid_unwatch_grace_period_secs: 3600,
        }
    }
}

impl CleanupConfig {
    /// Load configuration from environment variables.
    ///
    /// - `CLEANUP_FALLBACK_INTERVAL_SECS` - Fallback check interval (default: 60)
    /// - `CLEANUP_UNWATCH_GRACE_PERIOD_SECS` - Grace period before unwatching (default: 60)
    /// - `CLEANUP_PAID_UNWATCH_GRACE_PERIOD_SECS` - Grace period after a payment
    ///   confirms before unwatching its address (default: 3600)
    pub fn from_env() -> Self {
        Self {
            fallback_interval_secs: std::env::var("CLEANUP_FALLBACK_INTERVAL_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(60),
            unwatch_grace_period_secs: std::env::var("CLEANUP_UNWATCH_GRACE_PERIOD_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(60),
            paid_unwatch_grace_period_secs: std::env::var("CLEANUP_PAID_UNWATCH_GRACE_PERIOD_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(3600),
        }
    }
}

/// Service that handles invoice cleanup tasks.
///
/// This service:
/// 1. Expires pending invoices past their expiration time
/// 2. Unwatches addresses for expired invoices (after grace period)
/// 3. Unwatches addresses for paid invoices
/// 4. Unwatches addresses for cancelled invoices
/// 5. Sends webhook notifications when invoices expire
///
/// Can be triggered in two ways:
/// 1. Event-triggered: Call `check_expired()` when block events arrive
/// 2. Timer-triggered: The `run()` loop checks periodically
pub struct InvoiceCleanupService<D: CleanupDataService, M: EVMMonitor, W: WebhookDataService = D> {
    data_service: Arc<D>,
    evm_monitor: Arc<M>,
    config: CleanupConfig,
    webhook_service: Option<Arc<WebhookService<W>>>,
    ws_broadcast: Option<Arc<WsBroadcast>>,
}

impl<D: CleanupDataService + 'static, M: EVMMonitor, W: WebhookDataService + 'static>
    InvoiceCleanupService<D, M, W>
{
    /// Create a new invoice cleanup service.
    pub fn new(
        data_service: Arc<D>,
        evm_monitor: Arc<M>,
        config: CleanupConfig,
        webhook_service: Option<Arc<WebhookService<W>>>,
        ws_broadcast: Option<Arc<WsBroadcast>>,
    ) -> Self {
        Self {
            data_service,
            evm_monitor,
            config,
            webhook_service,
            ws_broadcast,
        }
    }

    /// Check and expire invoices for a specific chain (triggers full check).
    ///
    /// Called by EventConsumer when block events arrive.
    /// With network-agnostic invoices, this triggers a check of all expired invoices.
    pub async fn check_chain(&self, _chain_id: u64) -> Result<u64, CleanupError> {
        self.check_expired().await
    }

    /// Check and expire all invoices that have passed their expiration time.
    ///
    /// Uses streaming to minimize memory usage.
    /// Expires invoices in pending, processing, or partially_paid states.
    #[allow(clippy::cognitive_complexity)] // streaming expiration with per-invoice error handling
    pub async fn check_expired(&self) -> Result<u64, CleanupError> {
        tracing::debug!("Checking expired invoices");

        let mut expired_count = 0u64;
        let mut stream = pin!(InvoiceReader::stream_expired_pending(&*self.data_service));

        while let Some(result) = stream.next().await {
            match result {
                Ok(invoice_id) => {
                    match InvoiceWriter::expire(&*self.data_service, &invoice_id).await {
                        Ok(true) => {
                            expired_count += 1;
                            tracing::debug!(
                                invoice_id = %invoice_id.as_str(),
                                "Expired invoice"
                            );
                            metrics::record_invoice_expired();
                            // Broadcast invoice expired via WebSocket
                            if let Some(ref ws) = self.ws_broadcast {
                                ws.send_for_invoice(
                                    &*self.data_service,
                                    &invoice_id,
                                    StatusUpdate::InvoiceStatus {
                                        invoice_id: invoice_id.as_str().to_string(),
                                        status: "expired".to_string(),
                                    },
                                )
                                .await;
                            }
                            // Queue webhook notification for expiration
                            self.queue_expiration_webhook(&invoice_id).await;
                        }
                        Ok(false) => {
                            // Invoice was already expired or status changed
                            tracing::trace!(
                                invoice_id = %invoice_id.as_str(),
                                "Invoice already processed"
                            );
                        }
                        Err(e) => {
                            tracing::warn!(
                                invoice_id = %invoice_id.as_str(),
                                error = %e,
                                "Failed to expire invoice"
                            );
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Error streaming expired invoice");
                }
            }
        }

        if expired_count > 0 {
            tracing::info!(expired_count, "Expired pending invoices");
        }

        Ok(expired_count)
    }

    /// Queue a webhook notification for an expired invoice.
    ///
    /// This is a non-blocking operation - errors are logged but don't stop expiration processing.
    async fn queue_expiration_webhook(&self, invoice_id: &types::InvoiceId) {
        let Some(webhook_service) = &self.webhook_service else {
            return;
        };

        // Fetch the invoice to get store_id and details
        let invoice = match InvoiceReader::get(&*self.data_service, invoice_id).await {
            Ok(Some(inv)) => inv,
            Ok(None) => {
                tracing::warn!(invoice_id = %invoice_id.as_str(), "Invoice not found for webhook");
                return;
            }
            Err(e) => {
                tracing::warn!(invoice_id = %invoice_id.as_str(), error = %e, "Failed to fetch invoice for webhook");
                return;
            }
        };

        let store_id = invoice.store_id.0;
        let payload = WebhookPayload::invoice_event(WebhookEventType::InvoiceExpired, &invoice);
        queue_for_store(
            webhook_service.as_ref(),
            &*self.data_service,
            store_id,
            payload,
        )
        .await
        .warn_on_failure(invoice_id.as_str(), "invoice_expired");
    }

    /// Cleanup addresses for completed invoices.
    ///
    /// This method:
    /// 1. Unwatches addresses for expired invoices past grace period
    /// 2. Unwatches addresses for paid invoices
    /// 3. Unwatches addresses for cancelled invoices
    pub async fn cleanup_addresses(&self) -> Result<CleanupStats, CleanupError> {
        let stats = CleanupStats {
            expired: self.cleanup_expired_addresses().await?,
            paid: self.cleanup_paid_addresses().await?,
            cancelled: self.cleanup_cancelled_addresses().await?,
        };

        if stats.total() > 0 {
            tracing::info!(
                expired = stats.expired,
                paid = stats.paid,
                cancelled = stats.cancelled,
                "Cleaned up watched addresses"
            );
        }

        Ok(stats)
    }

    /// Cleanup addresses for expired invoices past grace period.
    async fn cleanup_expired_addresses(&self) -> Result<u64, CleanupError> {
        let addresses = WatchedAddressReader::get_expired_for_cleanup(
            &*self.data_service,
            self.config.unwatch_grace_period_secs as i64,
        )
        .await?;

        let mut count = 0u64;
        for info in addresses {
            if let Err(e) = self
                .unwatch_and_deactivate(
                    &info.address,
                    &info.chain_id,
                    info.token_address.as_deref(),
                )
                .await
            {
                tracing::warn!(
                    address = %info.address,
                    invoice_id = %info.invoice_id,
                    chain_id = %info.chain_id,
                    error = %e,
                    "Failed to cleanup expired address"
                );
            } else {
                tracing::debug!(
                    address = %info.address,
                    invoice_id = %info.invoice_id,
                    chain_id = %info.chain_id,
                    "Unwatched expired invoice address"
                );
                count += 1;
            }
        }

        Ok(count)
    }

    /// Cleanup addresses for paid invoices.
    ///
    /// Skips an address whose invoice has a payment that confirmed within
    /// `paid_unwatch_grace_period_secs` — see the field doc for why.
    #[allow(clippy::cognitive_complexity)] // per-item grace-period check + error handling, same shape as the loops above
    async fn cleanup_paid_addresses(&self) -> Result<u64, CleanupError> {
        let addresses = WatchedAddressReader::get_paid_for_cleanup(&*self.data_service).await?;

        let mut count = 0u64;
        for info in addresses {
            match self.within_paid_grace_period(&info.invoice_id).await {
                Ok(true) => continue,
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!(
                        invoice_id = %info.invoice_id,
                        error = %e,
                        "Failed to check paid-address grace period; leaving address watched"
                    );
                    continue;
                }
            }

            if let Err(e) = self
                .unwatch_and_deactivate(
                    &info.address,
                    &info.chain_id,
                    info.token_address.as_deref(),
                )
                .await
            {
                tracing::warn!(
                    address = %info.address,
                    invoice_id = %info.invoice_id,
                    chain_id = %info.chain_id,
                    error = %e,
                    "Failed to cleanup paid address"
                );
            } else {
                tracing::debug!(
                    address = %info.address,
                    invoice_id = %info.invoice_id,
                    chain_id = %info.chain_id,
                    "Unwatched paid invoice address"
                );
                count += 1;
            }
        }

        Ok(count)
    }

    /// Whether `invoice_id` has a payment that confirmed within its
    /// effective `paid_unwatch_grace_period_secs` of now.
    async fn within_paid_grace_period(&self, invoice_id: &str) -> Result<bool, CleanupError> {
        let payments = PaymentReader::get_for_invoice(
            &*self.data_service,
            &InvoiceId::from_string(invoice_id.to_string()),
        )
        .await?;

        let now = chrono::Utc::now();
        Ok(payments.iter().any(|p| {
            p.confirmed_at.is_some_and(|c| {
                let grace = chrono::Duration::seconds(
                    self.effective_paid_unwatch_grace_period_secs(&p.chain_id) as i64,
                );
                now - c < grace
            })
        }))
    }

    /// The configured `paid_unwatch_grace_period_secs`, raised to this
    /// chain's own minimum if the configured value is too thin for it — see
    /// `ChainConfig::min_paid_unwatch_grace_period_secs` for why. A single
    /// flat default tuned for one chain isn't automatically adequate for
    /// every chain this server watches, so the floor is derived per chain
    /// rather than left an implicit constant.
    fn effective_paid_unwatch_grace_period_secs(&self, chain_id: &types::ChainId) -> u64 {
        let floor = chain_id
            .evm_chain_id()
            .and_then(evm::get_any_chain_config)
            .map(evm::ChainConfig::min_paid_unwatch_grace_period_secs)
            .unwrap_or(0);
        self.config.paid_unwatch_grace_period_secs.max(floor)
    }

    /// Cleanup addresses for cancelled invoices.
    async fn cleanup_cancelled_addresses(&self) -> Result<u64, CleanupError> {
        let addresses =
            WatchedAddressReader::get_cancelled_for_cleanup(&*self.data_service).await?;

        let mut count = 0u64;
        for info in addresses {
            if let Err(e) = self
                .unwatch_and_deactivate(
                    &info.address,
                    &info.chain_id,
                    info.token_address.as_deref(),
                )
                .await
            {
                tracing::warn!(
                    address = %info.address,
                    invoice_id = %info.invoice_id,
                    chain_id = %info.chain_id,
                    error = %e,
                    "Failed to cleanup cancelled address"
                );
            } else {
                tracing::debug!(
                    address = %info.address,
                    invoice_id = %info.invoice_id,
                    chain_id = %info.chain_id,
                    "Unwatched cancelled invoice address"
                );
                count += 1;
            }
        }

        Ok(count)
    }

    /// Send the unwatch command, then deactivate the address in the database.
    ///
    /// Redis before Postgres, deliberately kept this way and not reordered
    /// to close the tiny cancelled-branch false-"missed" race a review
    /// raised (see the `expected_watched_addresses` migration's comment on
    /// why the `cancelled` branch has no grace window). Deactivating first
    /// was tried and reverted: it makes `is_active` flip unconditionally
    /// before the unwatch is even attempted, which (a) drops the retry a
    /// failed unwatch currently gets for free - the next cleanup poll only
    /// re-selects rows still `is_active = TRUE` - turning a self-healing
    /// failure into a permanently orphaned Redis watch, and (b) widens the
    /// window in which Postgres already reports an address inactive while
    /// Redis, which only learns about the unwatch asynchronously over
    /// pub/sub, is still actually watching it: `event_consumer`'s payment
    /// handler looks up the payment option through an `is_active`-gated
    /// query, so a payment landing in that window is detected but not
    /// credited. That is strictly worse than the race being traded away,
    /// which requires the monitor to consume and apply the unwatch message
    /// faster than this function's own subsequent Postgres write commits -
    /// a narrow ordering between two calls inside one function, not a
    /// window that persists for any observable duration. Trading a real,
    /// money-adjacent regression for a narrower detection-only false
    /// positive is the wrong side of that trade.
    async fn unwatch_and_deactivate(
        &self,
        address: &str,
        chain_id: &types::ChainId,
        token_address: Option<&str>,
    ) -> Result<(), CleanupError> {
        // Parse address
        let addr: Address = address
            .parse()
            .map_err(|_| CleanupError::InvalidAddress(address.to_string()))?;

        // Parse token contract address
        let token_contract: Option<Address> = token_address.and_then(|t| t.parse().ok());

        // The monitor is EVM-only and its RPCs take an EIP-155 number.
        let eip155 = chain_id
            .evm_chain_id()
            .ok_or_else(|| CleanupError::NotAnEvmChain(chain_id.to_string()))?;

        self.evm_monitor
            .unwatch_address_by_chain_id(eip155, addr, token_contract)
            .await?;

        // Deactivate in database
        WatchedAddressWriter::deactivate(&*self.data_service, address, chain_id, token_address)
            .await?;

        Ok(())
    }

    /// Run the cleanup service as a background task.
    ///
    /// This runs a periodic timer that:
    /// 1. Expires pending invoices
    /// 2. Cleans up watched addresses for completed invoices
    ///
    /// Should be spawned with `tokio::spawn(service.run())`.
    #[allow(clippy::cognitive_complexity)] // cleanup loop with timer + expiration + unwatch passes
    pub async fn run(self: Arc<Self>) {
        tracing::info!(
            interval_secs = self.config.fallback_interval_secs,
            grace_period_secs = self.config.unwatch_grace_period_secs,
            "Starting invoice cleanup service"
        );

        let mut interval = tokio::time::interval(std::time::Duration::from_secs(
            self.config.fallback_interval_secs,
        ));

        loop {
            interval.tick().await;

            // Expire pending invoices
            match self.check_expired().await {
                Ok(count) => {
                    if count > 0 {
                        tracing::info!(expired_count = count, "Expired invoices");
                    }
                }
                Err(e) => {
                    tracing::error!(error = %e, "Failed to expire invoices");
                }
            }

            // Cleanup watched addresses
            match self.cleanup_addresses().await {
                Ok(stats) => {
                    if stats.total() > 0 {
                        tracing::debug!(
                            expired = stats.expired,
                            paid = stats.paid,
                            cancelled = stats.cancelled,
                            "Address cleanup complete"
                        );
                    }
                }
                Err(e) => {
                    tracing::error!(error = %e, "Failed to cleanup addresses");
                }
            }
        }
    }
}

/// Statistics from address cleanup.
#[derive(Debug, Default)]
pub struct CleanupStats {
    /// Number of addresses cleaned up for expired invoices.
    pub expired: u64,
    /// Number of addresses cleaned up for paid invoices.
    pub paid: u64,
    /// Number of addresses cleaned up for cancelled invoices.
    pub cancelled: u64,
}

impl CleanupStats {
    /// Total number of addresses cleaned up.
    pub fn total(&self) -> u64 {
        self.expired + self.paid + self.cancelled
    }
}

/// Errors that can occur during cleanup operations.
#[derive(Debug, thiserror::Error)]
pub enum CleanupError {
    #[error("not an EVM chain: {0}")]
    NotAnEvmChain(String),
    #[error("Repository error: {0}")]
    Repository(#[from] types::RepositoryError),

    #[error("Monitor error: {0}")]
    Monitor(String),

    #[error("Invalid address: {0}")]
    InvalidAddress(String),
}

impl From<Box<dyn std::error::Error + Send + Sync>> for CleanupError {
    fn from(e: Box<dyn std::error::Error + Send + Sync>) -> Self {
        CleanupError::Monitor(e.to_string())
    }
}

impl From<super::evm_monitor::EVMMonitorError> for CleanupError {
    fn from(e: super::evm_monitor::EVMMonitorError) -> Self {
        CleanupError::Monitor(e.to_string())
    }
}

#[cfg(test)]
mod tests {
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

        async fn get_chain_health(
            &self,
        ) -> Result<Vec<evm::monitor::ChainHealth>, EVMMonitorError> {
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

        async fn get_chain_health(
            &self,
        ) -> Result<Vec<evm::monitor::ChainHealth>, EVMMonitorError> {
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
}
