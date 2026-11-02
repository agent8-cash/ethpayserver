//! Invoice expiration: marks overdue invoices expired and queues webhooks.

use std::pin::pin;

use futures::StreamExt;
use types::{InvoiceReader, InvoiceWriter};

use super::{CleanupDataService, CleanupError, InvoiceCleanupService};
use crate::api::ws::StatusUpdate;
use crate::metrics;
use crate::services::evm_monitor::EVMMonitor;
use crate::services::webhook::{
    WebhookDataService, WebhookEventType, WebhookPayload, queue_for_store,
};

impl<D: CleanupDataService + 'static, M: EVMMonitor, W: WebhookDataService + 'static>
    InvoiceCleanupService<D, M, W>
{
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
}
