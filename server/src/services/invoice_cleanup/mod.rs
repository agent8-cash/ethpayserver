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

mod addresses;
mod expiration;

use std::sync::Arc;

use data_service::StoreWebhookReader;
use types::{
    InvoiceReader, InvoiceWriter, PaymentReader, StoreSettingsReader, WatchedAddressReader,
    WatchedAddressWriter,
};

use crate::api::ws::WsBroadcast;

use super::evm_monitor::EVMMonitor;
use super::webhook::{WebhookDataService, WebhookService};

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
mod tests;
