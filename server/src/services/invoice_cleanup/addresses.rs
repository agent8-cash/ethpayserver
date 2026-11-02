//! Watched-address cleanup for expired, paid, and cancelled invoices.

use evm::Address;
use types::{InvoiceId, PaymentReader, WatchedAddressReader, WatchedAddressWriter};

use super::{CleanupDataService, CleanupError, CleanupStats, InvoiceCleanupService};
use crate::services::evm_monitor::EVMMonitor;
use crate::services::webhook::WebhookDataService;

impl<D: CleanupDataService + 'static, M: EVMMonitor, W: WebhookDataService + 'static>
    InvoiceCleanupService<D, M, W>
{
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
}
