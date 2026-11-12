//! Handler for `PaymentConfirmed` events and customer receipt emails.

use auth::StoreRepository;
use bigdecimal::BigDecimal;
use chrono::Utc;
use data_service::{PaymentTxIndexReader, SettlementToleranceReader, SettlementToleranceWriter};
use evm::get_any_chain_config;
use evm::monitor::events::PaymentConfirmed;
use types::{
    InvoiceData, InvoiceId, InvoiceReader, InvoiceStatus, InvoiceWriter, PaymentData,
    PaymentWriter, StoreSettingsReader,
};

use crate::api::ws::StatusUpdate;
use crate::metrics;
use crate::services::email::ReceiptData;
use crate::services::evm_monitor::EVMMonitor;
use crate::services::plugins::notify_own_store_payment;
use crate::services::settlement;
use crate::services::webhook::WebhookDataService;
use crate::services::webhook::WebhookEventType;

use super::{EventConsumer, EventConsumerDataService, EventConsumerError};

impl<
    D: EventConsumerDataService + 'static,
    M: EVMMonitor + 'static,
    W: WebhookDataService + 'static,
> EventConsumer<D, M, W>
{
    /// Record the shortfall a tolerance accepted, if the payment was short.
    ///
    /// Called only from the arms that actually move the invoice to a settled
    /// state, so a confirmation that settles nothing leaves no record. It runs
    /// before the status write and replaces any earlier record: a retry after
    /// a failed write re-records under the setting then in force, so the audit
    /// always names the setting that decided the outcome.
    async fn record_allowance_if_short(
        &self,
        invoice_id: &InvoiceId,
        amount_received: &BigDecimal,
        amount_expected: &BigDecimal,
        tolerance_percent: &BigDecimal,
        tolerance_source: &'static str,
    ) -> Result<(), EventConsumerError> {
        if amount_received >= amount_expected {
            return Ok(());
        }
        let shortfall = amount_expected - amount_received;
        SettlementToleranceWriter::record_settlement_allowance(
            &*self.data_service,
            invoice_id,
            &shortfall.to_string(),
            &tolerance_percent.to_string(),
            tolerance_source,
        )
        .await?;
        tracing::warn!(
            invoice_id = %invoice_id.as_str(),
            shortfall = %shortfall,
            tolerance_percent = %tolerance_percent,
            tolerance_source,
            "Invoice settled within the store's shortfall tolerance"
        );
        Ok(())
    }

    /// Handle PaymentConfirmed event.
    ///
    /// Updates payment confirmation status and transitions invoice to `paid`
    /// if amount_received >= amount.
    #[allow(clippy::too_many_lines, clippy::cognitive_complexity)] // confirmation state machine — reads/updates/transitions in one flow
    pub(super) async fn handle_payment_confirmed(
        &self,
        event: PaymentConfirmed,
    ) -> Result<(), EventConsumerError> {
        let chain_id = types::ChainId::evm(event.chain_id);
        let invoice_id = InvoiceId::from_string(event.invoice_id.to_string());
        let tx_hash = format!("{:#x}", event.tx_hash);

        // Find the payment by the transfer this confirms, not merely by its
        // transaction. `find(|p| p.tx_hash == tx_hash)` was well defined only
        // while the unique key guaranteed one row per (chain_id, tx_hash); two
        // transfers batched into one transaction now each have a row, and
        // picking the first would confirm an arbitrary one of them and leave
        // the other unconfirmed for good, since `mark_confirmed` is a no-op
        // once set. Still excludes reorged rows, as the previous reader did.
        let found = PaymentTxIndexReader::get_by_tx_index(
            &*self.data_service,
            &invoice_id,
            &tx_hash,
            event.tx_index,
        )
        .await?;
        let payment = match found.as_ref() {
            Some(p) => p,
            None => {
                // Payment not found or was reorged - log and skip
                tracing::debug!(
                    invoice_id = %event.invoice_id,
                    tx_hash = %tx_hash,
                    tx_index = event.tx_index,
                    "Payment not found or reorged, skipping confirmation"
                );
                return Ok(());
            }
        };

        // Mark payment as confirmed
        PaymentWriter::mark_confirmed(&*self.data_service, payment.id, event.confirmed_at).await?;

        tracing::info!(
            invoice_id = %event.invoice_id,
            tx_hash = %tx_hash,
            "Payment confirmed"
        );

        // Record metrics
        metrics::record_payment_confirmed(&chain_id, &payment.asset_symbol);

        // Record confirmation duration (detected_at → confirmed_at)
        if let Ok(duration) = (event.confirmed_at - payment.detected_at).to_std() {
            metrics::record_payment_confirmation_duration(
                &chain_id,
                &payment.asset_symbol,
                duration,
            );
        }

        // Check if invoice is fully paid
        let invoice = InvoiceReader::get(&*self.data_service, &invoice_id)
            .await?
            .ok_or_else(|| {
                EventConsumerError::InvalidData(format!("Invoice not found: {}", event.invoice_id))
            })?;

        // Compare amounts exactly. These columns are NUMERIC(78,18) and can
        // carry more significant digits than rust_decimal::Decimal's 96-bit
        // mantissa (~28-29 digits) can hold, so this uses the
        // arbitrary-precision BigDecimal instead - a fixed-mantissa type here
        // would silently round the amount that decides paid/underpaid/overpaid.
        let amount_received: BigDecimal = invoice.amount_received.parse().map_err(|e| {
            EventConsumerError::InvalidData(format!(
                "Invalid amount_received '{}': {}",
                invoice.amount_received, e
            ))
        })?;
        let amount_expected: BigDecimal = invoice.amount.parse().map_err(|e| {
            EventConsumerError::InvalidData(format!("Invalid amount '{}': {}", invoice.amount, e))
        })?;

        // The store's tolerance decides how close to the invoice amount
        // counts as paid; without one the server default applies, which is
        // never zero (see `DEFAULT_TOLERANCE_PERCENT`).
        let store_tolerance = SettlementToleranceReader::get_settlement_tolerance(
            &*self.data_service,
            invoice.store_id.0,
        )
        .await?;
        let (tolerance_percent, tolerance_source) =
            settlement::effective_tolerance(store_tolerance.as_deref())
                .map_err(EventConsumerError::InvalidData)?;
        let is_fully_paid =
            settlement::is_fully_paid(&amount_received, &amount_expected, &tolerance_percent);

        // Handle based on invoice status
        match invoice.status {
            InvoiceStatus::Processing | InvoiceStatus::PartiallyPaid => {
                // Normal flow: transition to paid if fully paid
                if is_fully_paid {
                    self.record_allowance_if_short(
                        &invoice_id,
                        &amount_received,
                        &amount_expected,
                        &tolerance_percent,
                        tolerance_source,
                    )
                    .await?;
                    InvoiceWriter::update_status(
                        &*self.data_service,
                        &invoice_id,
                        InvoiceStatus::Paid,
                    )
                    .await?;
                    tracing::info!(
                        invoice_id = %event.invoice_id,
                        amount_received = %amount_received,
                        amount_expected = %amount_expected,
                        "Invoice fully paid"
                    );
                    metrics::record_invoice_paid();

                    // Broadcast invoice paid via WebSocket
                    if let Some(ref ws) = self.ws_broadcast {
                        ws.send(
                            invoice.store_id,
                            StatusUpdate::InvoiceStatus {
                                invoice_id: event.invoice_id.to_string(),
                                status: InvoiceStatus::Paid.to_string(),
                            },
                        );
                    }

                    // Queue webhook notification for payment confirmed
                    if let Ok(Some(updated_invoice)) =
                        InvoiceReader::get(&*self.data_service, &invoice_id).await
                    {
                        self.queue_webhook(
                            WebhookEventType::PaymentConfirmed,
                            &updated_invoice,
                            Some(payment),
                        )
                        .await
                        .warn_on_failure(&event.invoice_id.to_string(), "payment_confirmed");
                    }

                    // Send customer receipt email (best-effort, never blocks payment flow)
                    self.send_customer_receipt(&invoice, payment, event.chain_id)
                        .await;

                    // Tell any plugin that this instance's own store settled
                    // an invoice - how a subscription learns it was paid.
                    // Deliberately last, after every write and every
                    // merchant-visible transition: it observes committed work
                    // and can neither refuse nor alter it. Filtered to our own
                    // store inside, never a merchant's.
                    self.notify_own_store_settled(&invoice_id, event.confirmed_at)
                        .await;
                }
            }
            InvoiceStatus::Expired => {
                // Late payment: invoice expired but payment still came through
                // Transition to LatePaid for merchant review
                if is_fully_paid {
                    self.record_allowance_if_short(
                        &invoice_id,
                        &amount_received,
                        &amount_expected,
                        &tolerance_percent,
                        tolerance_source,
                    )
                    .await?;
                    InvoiceWriter::update_status(
                        &*self.data_service,
                        &invoice_id,
                        InvoiceStatus::LatePaid,
                    )
                    .await?;
                    tracing::warn!(
                        invoice_id = %event.invoice_id,
                        amount_received = %amount_received,
                        amount_expected = %amount_expected,
                        "Late payment received on expired invoice - requires merchant review"
                    );

                    // Broadcast late payment via WebSocket
                    if let Some(ref ws) = self.ws_broadcast {
                        ws.send(
                            invoice.store_id,
                            StatusUpdate::InvoiceStatus {
                                invoice_id: event.invoice_id.to_string(),
                                status: InvoiceStatus::LatePaid.to_string(),
                            },
                        );
                    }

                    // Queue webhook notification for late payment
                    if let Ok(Some(updated_invoice)) =
                        InvoiceReader::get(&*self.data_service, &invoice_id).await
                    {
                        self.queue_webhook(
                            WebhookEventType::LatePaid,
                            &updated_invoice,
                            Some(payment),
                        )
                        .await
                        .warn_on_failure(&event.invoice_id.to_string(), "late_paid");
                    }

                    // A late payment is still money received - a subscription
                    // paid after its invoice expired has been paid.
                    self.notify_own_store_settled(&invoice_id, event.confirmed_at)
                        .await;
                }
            }
            _ => {
                // Cancelled, Refunded, LatePaid, or already Paid - don't modify
                tracing::debug!(
                    invoice_id = %event.invoice_id,
                    status = ?invoice.status,
                    "Invoice in final state, skipping status update"
                );
            }
        }

        Ok(())
    }

    /// Re-read the invoice in its settled state and report it to capability-4
    /// observers, if it belongs to this instance's own store.
    ///
    /// Re-reads rather than reusing the pre-transition copy: the status and
    /// `amount_received` an observer is told must be the ones that were
    /// committed, not the ones that were true before the update. A failed
    /// re-read is logged and dropped - this is an observation, and losing one
    /// must never fail a payment. That is exactly why a consumer of this
    /// capability has to reconcile through `OwnStorePaymentReader` rather than
    /// trust these dispatches.
    async fn notify_own_store_settled(
        &self,
        invoice_id: &InvoiceId,
        settled_at: chrono::DateTime<Utc>,
    ) {
        if self.payment_observers.is_empty() {
            return;
        }

        match InvoiceReader::get(&*self.data_service, invoice_id).await {
            Ok(Some(settled)) => {
                notify_own_store_payment(
                    &self.payment_observers,
                    self.own_store_id,
                    &settled,
                    settled_at,
                )
                .await;
            }
            Ok(None) => tracing::warn!(
                %invoice_id,
                "invoice vanished between its settled transition and the plugin notification"
            ),
            Err(e) => tracing::warn!(
                %invoice_id,
                error = %e,
                "could not re-read a settled invoice to notify plugins; reconciliation will catch it"
            ),
        }
    }

    /// Send a payment receipt email to the customer (best-effort).
    ///
    /// Checks: customer_email in metadata, store's customer_receipts_enabled toggle.
    /// Errors are logged but never propagated — email must not block the payment flow.
    pub(super) async fn send_customer_receipt(
        &self,
        invoice: &InvoiceData,
        payment: &PaymentData,
        chain_id: u64,
    ) {
        let Some(email) = Self::extract_customer_email(invoice) else {
            return;
        };

        if self.receipts_disabled_for_store(invoice.store_id.0).await {
            return;
        }

        let merchant_name = match StoreRepository::get_store(
            &*self.data_service,
            types::StoreId(invoice.store_id.0),
        )
        .await
        {
            Ok(Some(store)) => store.name,
            _ => "Merchant".to_string(),
        };

        let explorer_url = get_any_chain_config(chain_id)
            .map(|cfg| cfg.tx_explorer_url(&payment.tx_hash))
            .unwrap_or_else(|| payment.tx_hash.clone());

        let receipt = ReceiptData {
            invoice_id: invoice.id.as_str().to_string(),
            amount: invoice.amount.clone(),
            currency: invoice.currency.clone(),
            tx_hash: payment.tx_hash.clone(),
            explorer_url,
            paid_at: payment.confirmed_at.unwrap_or_else(Utc::now),
            merchant_name,
        };

        if let Err(e) = self.email_sender.send_receipt(email, &receipt).await {
            tracing::warn!(
                invoice_id = %invoice.id.as_str(),
                customer_email = %email,
                error = %e,
                "Failed to send customer receipt email"
            );
        } else {
            tracing::info!(
                invoice_id = %invoice.id.as_str(),
                customer_email = %email,
                "Customer receipt email sent"
            );
        }
    }

    /// Extract customer email from invoice metadata.
    /// The address a receipt goes to.
    ///
    /// Prefers the dedicated column, falling back to `metadata` for invoices
    /// created before it existed. The fallback is not decoration: new
    /// writes populate the column and no longer put the address in metadata, so
    /// reading metadata alone would have stopped receipts for every new invoice
    /// - silently, since a missing address is a normal, unlogged case here.
    pub(super) fn extract_customer_email(invoice: &InvoiceData) -> Option<&str> {
        invoice.customer_email.as_deref().or_else(|| {
            invoice
                .metadata
                .as_ref()
                .and_then(|m| m.get("customer_email").or_else(|| m.get("buyer_email")))
                .and_then(|v| v.as_str())
        })
    }

    /// Check if customer receipts are disabled for the given store.
    pub(super) async fn receipts_disabled_for_store(&self, store_id: uuid::Uuid) -> bool {
        if let Ok(Some(settings)) =
            StoreSettingsReader::get_store_settings(&*self.data_service, store_id).await
            && settings.notification_prefs.get("customer_receipts_enabled")
                == Some(&serde_json::Value::Bool(false))
        {
            tracing::trace!(
                store_id = %store_id,
                "Customer receipts disabled for store"
            );
            return true;
        }
        false
    }
}
