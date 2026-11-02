//! Handler for `ReorgDetected` events.

use std::collections::HashMap;

use evm::monitor::events::ReorgDetected;
use types::{InvoiceId, InvoiceReader, InvoiceStatus, InvoiceWriter, PaymentData, PaymentReader};

use crate::api::ws::StatusUpdate;
use crate::services::evm_monitor::EVMMonitor;
use crate::services::webhook::{WebhookDataService, WebhookPayload};

use super::{EventConsumer, EventConsumerDataService, EventConsumerError};

impl<
    D: EventConsumerDataService + 'static,
    M: EVMMonitor + 'static,
    W: WebhookDataService + 'static,
> EventConsumer<D, M, W>
{
    /// Handle ReorgDetected event.
    ///
    /// The candidate set comes from the database (`ReorgCandidateReader`),
    /// not `event.affected_invoices`. That list is the monitor's in-memory
    /// pending-payment map: it is empty right after a monitor restart, and it
    /// never includes a payment that has already confirmed and dropped out
    /// of that map. Either hole left an invoice stuck `paid` for money that
    /// had already been reorged away. The database is what survives both.
    ///
    /// `event.survived_tx_hashes` are transactions the monitor re-validated
    /// against the chain and found still present, merely relocated to a
    /// different block by the reorg. Those are excluded from retraction —
    /// retracting a payment that is genuinely still paid is the opposite
    /// error, and it un-pays an invoice a customer actually settled.
    ///
    /// Marks affected payments as reorged and reverts invoice status:
    /// - If other valid payments exist → `processing`
    /// - If no valid payments → `pending`
    ///
    /// Emits `payment_reorged` for each invoice whose payments were
    /// retracted. A subscriber was told about those payments and has no
    /// other way to learn they are gone: the invoice silently moving
    /// backwards is not a notification.
    #[allow(clippy::cognitive_complexity)]
    pub(super) async fn handle_reorg_detected(
        &self,
        event: ReorgDetected,
    ) -> Result<(), EventConsumerError> {
        let chain_id = types::ChainId::evm(event.chain_id);

        // An empty survivor list from a scan that never ran looks identical
        // to "everything was dropped", and retracting on it would un-pay
        // every candidate payment above `fork_block`. An empty watch set no
        // longer causes this - the monitor's scan falls back to the chain's
        // own block contents when there's nothing to filter by address (see
        // `ChainMonitor::scan_all_transactions`) - so `false` here now means
        // a genuine failure to reach the chain while re-validating.
        //
        // The monitor can confirm a survivor; it can never prove an absence.
        // So absence is only actionable when the scan could actually look.
        if !event.survivors_verifiable {
            tracing::error!(
                chain_id = %chain_id,
                fork_block = event.fork_block,
                depth = event.depth,
                "reorg detected but its survivors could not be verified (chain unreachable \
                 while re-validating); no payment is retracted. Any payment the chain really \
                 dropped stays marked valid until the next reorg re-validates it."
            );
            return Ok(());
        }

        let candidates = self
            .data_service
            .reorg_candidates(&chain_id, event.fork_block)
            .await?;

        tracing::warn!(
            chain_id = %chain_id,
            fork_block = event.fork_block,
            depth = event.depth,
            candidates = candidates.len(),
            "Chain reorganization detected"
        );

        let mut by_invoice: HashMap<InvoiceId, Vec<PaymentData>> = HashMap::new();
        for payment in candidates {
            let survived = event
                .survived_tx_hashes
                .iter()
                .any(|hash| tx_hash_eq(hash, &payment.tx_hash));
            if survived {
                continue;
            }
            by_invoice
                .entry(payment.invoice_id.clone())
                .or_default()
                .push(payment);
        }

        for (invoice_id, mut to_retract) in by_invoice {
            for payment in &mut to_retract {
                self.data_service.mark_payment_reorged(payment.id).await?;
                // Mirror the write we just made, rather than re-reading: the
                // webhook payload below reports what this reorg just did.
                payment.reorged = true;
                payment.confirmed_at = None;
            }

            tracing::info!(
                invoice_id = %invoice_id,
                reorged_count = to_retract.len(),
                fork_block = event.fork_block,
                "Marked payments as reorged"
            );

            // Determine new invoice status based on remaining valid payments
            let has_valid =
                PaymentReader::has_valid_payments(&*self.data_service, &invoice_id).await?;
            let new_status = if has_valid {
                InvoiceStatus::Processing
            } else {
                InvoiceStatus::Pending
            };

            let new_status = self
                .apply_reorg_status(&invoice_id, new_status, event.fork_block)
                .await?;

            // Broadcast reorg-induced status change via WebSocket
            if let Some(ref ws) = self.ws_broadcast {
                ws.send_for_invoice(
                    &*self.data_service,
                    &invoice_id,
                    StatusUpdate::InvoiceStatus {
                        invoice_id: invoice_id.as_str().to_string(),
                        status: new_status.to_string(),
                    },
                )
                .await;
            }

            tracing::info!(
                invoice_id = %invoice_id,
                new_status = ?new_status,
                "Reverted invoice status after reorg"
            );

            self.notify_reorg(&invoice_id, &to_retract).await;
        }

        Ok(())
    }

    /// Tell the store's webhook subscriber to forget the retracted payments.
    ///
    /// The invoice is re-read so the payload reports the status and received
    /// amount the subscriber is being moved *to*, not the ones it already
    /// knows are wrong.
    async fn notify_reorg(&self, invoice_id: &InvoiceId, retracted: &[PaymentData]) {
        if retracted.is_empty() || self.webhook_service.is_none() {
            return;
        }

        let invoice = match InvoiceReader::get(&*self.data_service, invoice_id).await {
            Ok(Some(invoice)) => invoice,
            Ok(None) => {
                tracing::warn!(
                    invoice_id = %invoice_id.as_str(),
                    "Invoice vanished before the reorg webhook could be built"
                );
                return;
            }
            Err(e) => {
                tracing::warn!(
                    invoice_id = %invoice_id.as_str(),
                    error = %e,
                    "Failed to re-read invoice for the reorg webhook"
                );
                return;
            }
        };

        let store_id = invoice.store_id.0;
        let payload = WebhookPayload::payment_reorged(&invoice, retracted);
        self.queue_payload(store_id, payload)
            .await
            .warn_on_failure(invoice_id.as_str(), "payment_reorged");
    }
    /// Write the post-reorg invoice status, unless the invoice is closed.
    ///
    /// Some statuses must not be walked backwards, and
    /// `InvoiceWriter::update_status` will not stop it - it is a bare
    /// `UPDATE invoices SET status` with no state guard.
    ///
    /// This did not matter while the reorg loop ran over the monitor's
    /// `pending` map, which never held a confirmed payment. Making confirmed
    /// payments reorg candidates puts settled invoices in reach: a reorg at or
    /// below a refunded payment's block would otherwise flip that invoice to
    /// `Pending` - payable again, with the refund the merchant already sent
    /// from their own wallet orphaned against it. `Cancelled` has the same
    /// shape, and an expired invoice must not reopen because a payment that
    /// arrived for it was retracted.
    ///
    /// `Paid` is deliberately not treated as closed. Un-paying an invoice
    /// whose payment the chain retracted is exactly what this handler exists
    /// to do; that is a correction, not a reopening.
    ///
    /// Returns the status the invoice is actually left in, which the caller
    /// reports onward.
    async fn apply_reorg_status(
        &self,
        invoice_id: &InvoiceId,
        computed: InvoiceStatus,
        fork_block: u64,
    ) -> Result<InvoiceStatus, EventConsumerError> {
        let current = InvoiceReader::get(&*self.data_service, invoice_id)
            .await?
            .ok_or_else(|| {
                EventConsumerError::InvalidData(format!("Invoice not found: {invoice_id}"))
            })?;

        let closed = matches!(
            current.status,
            InvoiceStatus::Refunded
                | InvoiceStatus::Cancelled
                | InvoiceStatus::Expired
                | InvoiceStatus::LatePaid
        );

        if closed {
            tracing::warn!(
                invoice_id = %invoice_id,
                status = ?current.status,
                would_have_been = ?computed,
                fork_block,
                "reorg retracted a payment on a closed invoice; the payments are \
                 marked reorged but the invoice status is left alone"
            );
            return Ok(current.status);
        }

        InvoiceWriter::update_status(&*self.data_service, invoice_id, computed).await?;
        Ok(computed)
    }
}

/// Whether `hash`, as the monitor observed it on chain, names the same
/// transaction as `stored`, as persisted in `payments.tx_hash`.
///
/// Parses `stored` back into a `B256` and compares bytes, rather than
/// formatting `hash` to a string and comparing text: two independently
/// formatted strings degrade silently the moment either representation
/// drifts (case, `0x` prefix, …), and a silent mismatch here means "nothing
/// survived" — retracting a payment that is still genuinely on chain. A
/// parse failure means `stored` isn't a well-formed hash at all, which is a
/// data problem no string comparison would have caught either.
fn tx_hash_eq(hash: &evm::B256, stored: &str) -> bool {
    stored
        .parse::<evm::B256>()
        .is_ok_and(|parsed| parsed == *hash)
}
