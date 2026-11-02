//! Test utilities for data service.

mod invoice_creation;
mod store_payment_method;

use std::collections::{BTreeMap, HashMap};
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, Utc};
use futures::stream::{self, BoxStream, StreamExt};
use types::{
    ChainId, CleanupAddressInfo, InvoiceData, InvoiceId, InvoiceQueryParams, InvoiceReader,
    InvoiceStatus, InvoiceWriter, PaymentData, PaymentEventWriter, PaymentMethodId,
    PaymentOptionData, PaymentOptionId, PaymentOptionReader, PaymentOptionWriter,
    PaymentQueryParams, PaymentReader, PaymentWriter, PendingWatchInfo, RepositoryError,
    RepositoryResult, StoreId, StorePaymentMethod, StoreSettings, StoreSettingsReader,
    StoreWebhook, StoreWebhookReader, TokenData, TokenQueryParams, TokenReader, TokenWriter,
    WatchedAddressReader, WatchedAddressWriter,
};
use uuid::Uuid;

use crate::analytics::{
    PaymentAnalyticsReader, PaymentVolumeBucket, PaymentVolumeQuery, StorePaymentVolumeBucket,
};
use crate::chain_cursor::{ChainCursor, ChainCursorReader, ChainCursorWriter};
use crate::{UpsertDeliveryParams, WebhookDeliveryWriter};

/// In-memory implementation of all repository traits for testing.
#[derive(Default)]
pub struct InMemoryDataService {
    invoices: RwLock<HashMap<String, InvoiceData>>,
    payments: RwLock<HashMap<Uuid, PaymentData>>,
    settlement_tolerances: RwLock<HashMap<Uuid, String>>,
    settlement_allowances: RwLock<HashMap<String, crate::SettlementAllowance>>,
    // Key: (chain_id, tx_hash, tx_index) -> payment id. Mirrors the real
    // `unique_payment_tx` constraint, for `PaymentTxIndexWriter::upsert_with_tx_index`
    // below. The plain `PaymentWriter::upsert` above does not consult this index,
    // so (unlike Postgres, where both paths hit the same constraint) mixing the
    // two entry points for the same (chain_id, tx_hash) will not collide here.
    // Nothing in this crate does that.
    payment_tx_index: RwLock<HashMap<(ChainId, String, i32), Uuid>>,
    payment_options: RwLock<HashMap<Uuid, PaymentOptionData>>,
    // Key: (address, chain_id, token_address) -> payment_option_id
    addresses: RwLock<HashMap<(String, ChainId, Option<String>), PaymentOptionId>>,
    tokens: RwLock<HashMap<i64, TokenData>>,
    token_id_counter: RwLock<i64>,
    webhooks: RwLock<HashMap<Uuid, StoreWebhook>>,
    // Kept as a Vec so `get_enabled_payment_methods` returns a stable order.
    payment_methods: RwLock<Vec<StorePaymentMethod>>,
    chain_cursors: RwLock<HashMap<(String, u64), ChainCursor>>,
    // `reset_chain_watch_notifications` is otherwise a no-op here (see its
    // impl below), so a test asserting a lineage break actually re-armed
    // `watch_retry` has nothing else to check against.
    watch_reset_calls: AtomicU64,
    // Lets a test force `chain_cursors` to fail, to exercise how a consumer
    // reacts to a DB error at startup - which is not the same thing as an
    // empty result (see `ChainCursorReader::chain_cursors`'s own doc
    // comment), and nothing else in this test double can produce it.
    fail_chain_cursors: AtomicBool,
    // Lets a test force `reset_chain_watch_notifications` to fail, to
    // exercise a lineage break whose re-arm cannot be trusted to have
    // happened - the one case this test double cannot otherwise reach, since
    // it is normally a no-op that always succeeds.
    fail_reset_chain_watch_notifications: AtomicBool,
    // Lets a test force `commit_chain_cursor` to fail, to exercise a
    // consumer whose event applied cleanly but whose durable cursor write
    // did not - the one case this test double cannot otherwise reach, since
    // it is normally an infallible map insert.
    fail_commit_chain_cursor: AtomicBool,
    // Separate from the commit flag so a test can fail only the stale-row
    // delete that follows a successful re-arm.
    fail_delete_chain_cursor: AtomicBool,
    webhook_outbox: RwLock<Vec<crate::WebhookObligation>>,
    /// Obligation id -> claim deadline, mirroring the Postgres
    /// implementation's `claimed_until` column so this double's
    /// `claim_undispatched_obligations` enforces the same "invisible until
    /// the claim expires" rule a real concurrent-drain test would need.
    webhook_outbox_claims: RwLock<HashMap<Uuid, DateTime<Utc>>>,
    /// When set, `InvoiceReader::get`/`PaymentReader::get` return a transient
    /// error instead of consulting their maps, standing in for a database
    /// that is temporarily unreachable - there is no other way to exercise a
    /// caller's "retry rather than give up" branch against this double,
    /// since every other method here only ever returns `Ok`.
    fail_invoice_reads: AtomicBool,
    fail_payment_reads: AtomicBool,
    fail_payment_writes: AtomicBool,
}

impl InMemoryDataService {
    pub fn new() -> Self {
        Self::default()
    }

    /// How many times `reset_chain_watch_notifications` has been called.
    pub fn watch_reset_calls(&self) -> u64 {
        self.watch_reset_calls.load(Ordering::SeqCst)
    }

    /// Force the next (and every subsequent) `chain_cursors` call to fail.
    pub fn set_fail_chain_cursors(&self, fail: bool) {
        self.fail_chain_cursors.store(fail, Ordering::SeqCst);
    }

    /// Force the next (and every subsequent) `reset_chain_watch_notifications`
    /// call to fail.
    pub fn set_fail_reset_chain_watch_notifications(&self, fail: bool) {
        self.fail_reset_chain_watch_notifications
            .store(fail, Ordering::SeqCst);
    }

    /// Force the next (and every subsequent) `commit_chain_cursor` call to
    /// fail.
    pub fn set_fail_commit_chain_cursor(&self, fail: bool) {
        self.fail_commit_chain_cursor.store(fail, Ordering::SeqCst);
    }

    /// Force every subsequent `delete_chain_cursor` call to fail.
    pub fn set_fail_delete_chain_cursor(&self, fail: bool) {
        self.fail_delete_chain_cursor.store(fail, Ordering::SeqCst);
    }

    /// Make every subsequent payment upsert fail with a transient error.
    pub fn fail_payment_writes(&self) {
        self.fail_payment_writes.store(true, Ordering::SeqCst);
    }

    /// Make every subsequent `InvoiceReader::get` call fail with a transient
    /// error (for testing).
    pub fn fail_invoice_reads(&self) {
        self.fail_invoice_reads.store(true, Ordering::SeqCst);
    }

    /// Make every subsequent `PaymentReader::get` call fail with a transient
    /// error (for testing).
    pub fn fail_payment_reads(&self) {
        self.fail_payment_reads.store(true, Ordering::SeqCst);
    }

    /// Set up a webhook for a store (for testing).
    pub fn set_webhook(&self, store_id: Uuid, url: &str, secret: &str) {
        let mut webhooks = self.webhooks.write().unwrap();
        webhooks.insert(
            store_id,
            StoreWebhook {
                id: Uuid::new_v4(),
                store_id,
                webhook_url: url.to_string(),
                webhook_secret: secret.to_string(),
                enabled: true,
                created_at: Utc::now(),
                updated_at: Utc::now(),
            },
        );
    }
}

// =============================================================================
// List search
// =============================================================================
//
// These mirror the SQL in `postgres/{invoice,payment}.rs` column for column.
// The Postgres side lower-cases the term and the column and matches with
// `LIKE`, anchored (`term%`) or not (`%term%`); these are the Rust spelling of
// exactly that. A double that disagrees with the real store about a filter is
// how the mock and the store silently diverged before, so if one side
// changes, both change.

/// `LOWER(col) LIKE 'term%'`.
fn search_starts_with(haystack: Option<&str>, term: &str) -> bool {
    haystack.is_some_and(|h| h.to_lowercase().starts_with(term))
}

/// `LOWER(col) LIKE '%term%'`.
fn search_contains(haystack: Option<&str>, term: &str) -> bool {
    haystack.is_some_and(|h| h.to_lowercase().contains(term))
}

#[async_trait]
impl InvoiceReader for InMemoryDataService {
    async fn get(&self, id: &InvoiceId) -> RepositoryResult<Option<InvoiceData>> {
        if self.fail_invoice_reads.load(Ordering::SeqCst) {
            return Err(RepositoryError::Database(
                "simulated transient invoice read failure".to_string(),
            ));
        }
        let invoices = self.invoices.read().unwrap();
        Ok(invoices.get(&id.0).cloned())
    }

    async fn query(
        &self,
        params: &InvoiceQueryParams,
    ) -> RepositoryResult<(i64, Vec<InvoiceData>)> {
        let invoices = self.invoices.read().unwrap();
        let mut results: Vec<InvoiceData> = invoices
            .values()
            .filter(|inv| {
                if let Some(store_id) = params.store_id
                    && inv.store_id != store_id
                {
                    return false;
                }
                // Membership scoping must behave here exactly as it does in
                // Postgres, empty list included - a mock that disagrees with
                // the real store about a scoping rule is how that divergence
                // happened before.
                if let Some(ref store_ids) = params.store_ids
                    && !store_ids.contains(&inv.store_id)
                {
                    return false;
                }
                if let Some(status) = params.status
                    && inv.status != status
                {
                    return false;
                }
                if let Some(ref currency) = params.currency
                    && inv.currency != *currency
                {
                    return false;
                }
                // Search is ANDed on top of the store scope above, never in
                // place of it: a term that matches another tenant's invoice
                // still must not return it.
                if let Some(term) = params.search_term() {
                    let term = term.to_lowercase();
                    let metadata = inv.metadata.as_ref().map(ToString::to_string);
                    let matched = search_starts_with(Some(inv.id.0.as_str()), &term)
                        || search_contains(Some(inv.currency.as_str()), &term)
                        || search_contains(Some(inv.amount.as_str()), &term)
                        // TODO: drop this line with its Postgres twin when
                        // metadata is encrypted client-side.
                        || search_contains(metadata.as_deref(), &term);
                    if !matched {
                        return false;
                    }
                }
                true
            })
            .cloned()
            .collect();

        let total = results.len() as i64;
        results.sort_by_key(|b| std::cmp::Reverse(b.created_at));

        let offset = params.offset as usize;
        let limit = params.limit as usize;
        let results = results.into_iter().skip(offset).take(limit).collect();

        Ok((total, results))
    }

    async fn get_expired(&self) -> RepositoryResult<Vec<InvoiceData>> {
        let invoices = self.invoices.read().unwrap();
        let now = Utc::now();
        Ok(invoices
            .values()
            .filter(|inv| inv.status == InvoiceStatus::Pending && inv.expires_at < now)
            .cloned()
            .collect())
    }

    fn stream_expired_pending(&self) -> BoxStream<'_, RepositoryResult<InvoiceId>> {
        let invoices = self.invoices.read().unwrap();
        let now = Utc::now();
        let ids: Vec<RepositoryResult<InvoiceId>> = invoices
            .values()
            .filter(|inv| inv.status == InvoiceStatus::Pending && inv.expires_at < now)
            .map(|inv| Ok(inv.id.clone()))
            .collect();
        stream::iter(ids).boxed()
    }
}

#[async_trait]
impl InvoiceWriter for InMemoryDataService {
    async fn upsert(&self, invoice: &InvoiceData) -> RepositoryResult<()> {
        let mut invoices = self.invoices.write().unwrap();
        invoices.insert(invoice.id.0.clone(), invoice.clone());
        Ok(())
    }

    async fn update_status(&self, id: &InvoiceId, status: InvoiceStatus) -> RepositoryResult<()> {
        let mut invoices = self.invoices.write().unwrap();
        if let Some(invoice) = invoices.get_mut(&id.0) {
            invoice.status = status;
        }
        Ok(())
    }

    async fn update_amount_received(&self, id: &InvoiceId, amount: &str) -> RepositoryResult<()> {
        let mut invoices = self.invoices.write().unwrap();
        if let Some(invoice) = invoices.get_mut(&id.0) {
            invoice.amount_received = amount.to_string();
        }
        Ok(())
    }

    async fn expire(&self, id: &InvoiceId) -> RepositoryResult<bool> {
        let mut invoices = self.invoices.write().unwrap();
        if let Some(invoice) = invoices.get_mut(&id.0)
            && invoice.status == InvoiceStatus::Pending
        {
            invoice.status = InvoiceStatus::Expired;
            return Ok(true);
        }
        Ok(false)
    }
}

#[async_trait]
impl PaymentReader for InMemoryDataService {
    async fn get(&self, id: Uuid) -> RepositoryResult<Option<PaymentData>> {
        if self.fail_payment_reads.load(Ordering::SeqCst) {
            return Err(RepositoryError::Database(
                "simulated transient payment read failure".to_string(),
            ));
        }
        let payments = self.payments.read().unwrap();
        Ok(payments.get(&id).cloned())
    }

    async fn get_for_invoice(&self, invoice_id: &InvoiceId) -> RepositoryResult<Vec<PaymentData>> {
        let payments = self.payments.read().unwrap();
        Ok(payments
            .values()
            .filter(|p| p.invoice_id == *invoice_id)
            .cloned()
            .collect())
    }

    async fn get_awaiting_confirmation(&self) -> RepositoryResult<Vec<PaymentData>> {
        let payments = self.payments.read().unwrap();
        Ok(payments
            .values()
            .filter(|p| p.confirmed_at.is_none() && !p.reorged)
            .cloned()
            .collect())
    }

    async fn get_valid_for_invoice(
        &self,
        invoice_id: &InvoiceId,
    ) -> RepositoryResult<Vec<PaymentData>> {
        let payments = self.payments.read().unwrap();
        Ok(payments
            .values()
            .filter(|p| p.invoice_id == *invoice_id && !p.reorged)
            .cloned()
            .collect())
    }

    async fn has_valid_payments(&self, invoice_id: &InvoiceId) -> RepositoryResult<bool> {
        let payments = self.payments.read().unwrap();
        Ok(payments
            .values()
            .any(|p| p.invoice_id == *invoice_id && !p.reorged))
    }

    async fn query(
        &self,
        params: &PaymentQueryParams,
    ) -> RepositoryResult<(i64, Vec<PaymentData>)> {
        let payments = self.payments.read().unwrap();
        let invoices = self.invoices.read().unwrap();

        let mut filtered: Vec<PaymentData> = payments
            .values()
            .filter(|p| {
                if let Some(ref invoice_id) = params.invoice_id
                    && p.invoice_id != *invoice_id
                {
                    return false;
                }
                if let Some(store_id) = params.store_id {
                    let invoice_store_id = invoices.get(&p.invoice_id.0).map(|i| i.store_id);
                    if invoice_store_id != Some(store_id) {
                        return false;
                    }
                }
                // Same rule as Postgres, empty list included. A
                // payment whose invoice is missing belongs to no store the
                // caller can see, so it is filtered out rather than let through.
                if let Some(ref store_ids) = params.store_ids {
                    match invoices.get(&p.invoice_id.0).map(|i| i.store_id) {
                        Some(sid) if store_ids.contains(&sid) => {}
                        _ => return false,
                    }
                }
                if let Some(confirmed) = params.confirmed {
                    if confirmed && p.confirmed_at.is_none() {
                        return false;
                    }
                    if !confirmed && p.confirmed_at.is_some() {
                        return false;
                    }
                }
                // ANDed on top of the store scope above, never in place of it.
                if let Some(term) = params.search_term() {
                    let term = term.to_lowercase();
                    let matched = search_starts_with(Some(p.tx_hash.as_str()), &term)
                        || search_starts_with(Some(p.invoice_id.0.as_str()), &term)
                        || search_contains(Some(p.asset_symbol.as_str()), &term)
                        || search_contains(p.from_address.as_deref(), &term);
                    if !matched {
                        return false;
                    }
                }
                true
            })
            .cloned()
            .collect();

        filtered.sort_by_key(|b| std::cmp::Reverse(b.detected_at));
        let total = filtered.len() as i64;
        let offset = params.offset as usize;
        let limit = params.limit as usize;
        let page = filtered.into_iter().skip(offset).take(limit).collect();

        Ok((total, page))
    }
}

#[async_trait]
impl PaymentWriter for InMemoryDataService {
    async fn upsert(&self, payment: &PaymentData) -> RepositoryResult<()> {
        let mut payments = self.payments.write().unwrap();
        payments.insert(payment.id, payment.clone());
        // Record it on the transfer index too, at tx_index 0 - the same value
        // `PgDataService::upsert` passes for a caller that does not know which
        // transfer within the transaction this was. Without this the fake and
        // the real store disagree about whether a plainly-upserted payment is
        // findable by transfer, and a test would pass against a lookup that
        // returns nothing in production.
        self.payment_tx_index.write().unwrap().insert(
            (payment.chain_id.clone(), payment.tx_hash.clone(), 0),
            payment.id,
        );
        Ok(())
    }

    async fn mark_confirmed(&self, id: Uuid, confirmed_at: DateTime<Utc>) -> RepositoryResult<()> {
        let mut payments = self.payments.write().unwrap();
        if let Some(payment) = payments.get_mut(&id)
            && payment.confirmed_at.is_none()
        {
            payment.confirmed_at = Some(confirmed_at);
        }
        Ok(())
    }

    async fn mark_reorged(
        &self,
        invoice_id: &InvoiceId,
        chain_id: &types::ChainId,
        fork_block: u64,
    ) -> RepositoryResult<u64> {
        let mut payments = self.payments.write().unwrap();
        let mut count = 0u64;
        for payment in payments.values_mut() {
            if payment.invoice_id == *invoice_id
                && &payment.chain_id == chain_id
                && payment.block_number.is_some_and(|b| b >= fork_block)
                && !payment.reorged
            {
                payment.reorged = true;
                payment.confirmed_at = None;
                count += 1;
            }
        }
        Ok(count)
    }
}

#[async_trait]
impl crate::SettlementToleranceReader for InMemoryDataService {
    async fn get_settlement_tolerance(&self, store_id: Uuid) -> RepositoryResult<Option<String>> {
        Ok(self
            .settlement_tolerances
            .read()
            .unwrap()
            .get(&store_id)
            .cloned())
    }

    async fn get_settlement_allowance(
        &self,
        invoice_id: &types::InvoiceId,
    ) -> RepositoryResult<Option<crate::SettlementAllowance>> {
        Ok(self
            .settlement_allowances
            .read()
            .unwrap()
            .get(invoice_id.as_str())
            .cloned())
    }
}

#[async_trait]
impl crate::SettlementToleranceWriter for InMemoryDataService {
    async fn set_settlement_tolerance(
        &self,
        store_id: Uuid,
        tolerance_percent: &str,
    ) -> RepositoryResult<()> {
        self.settlement_tolerances
            .write()
            .unwrap()
            .insert(store_id, tolerance_percent.to_string());
        Ok(())
    }

    async fn clear_settlement_tolerance(&self, store_id: Uuid) -> RepositoryResult<()> {
        self.settlement_tolerances
            .write()
            .unwrap()
            .remove(&store_id);
        Ok(())
    }

    async fn record_settlement_allowance(
        &self,
        invoice_id: &types::InvoiceId,
        shortfall: &str,
        tolerance_percent: &str,
        source: &str,
    ) -> RepositoryResult<()> {
        self.settlement_allowances.write().unwrap().insert(
            invoice_id.as_str().to_string(),
            crate::SettlementAllowance {
                invoice_id: invoice_id.as_str().to_string(),
                shortfall: shortfall.to_string(),
                tolerance_percent: tolerance_percent.to_string(),
                source: source.to_string(),
                recorded_at: Utc::now(),
            },
        );
        Ok(())
    }
}

#[async_trait]
impl crate::payment_tx_index::PaymentTxIndexReader for InMemoryDataService {
    async fn get_by_tx_index(
        &self,
        invoice_id: &types::InvoiceId,
        tx_hash: &str,
        tx_index: i32,
    ) -> RepositoryResult<Option<PaymentData>> {
        // Resolved through `payment_tx_index` rather than by scanning
        // `payments` for a matching hash, so a test that writes two transfers
        // of one transaction sees them as two distinct rows here exactly as
        // Postgres does. A scan would find the first and make the collision
        // this models invisible.
        let index = self.payment_tx_index.read().unwrap();
        let payments = self.payments.read().unwrap();

        let found = index
            .iter()
            .filter(|((_, hash, idx), _)| hash == tx_hash && *idx == tx_index)
            .filter_map(|(_, id)| payments.get(id))
            .find(|p| p.invoice_id == *invoice_id && !p.reorged)
            .cloned();

        Ok(found)
    }
}

#[async_trait]
impl crate::payment_tx_index::PaymentTxIndexWriter for InMemoryDataService {
    async fn upsert_with_tx_index(
        &self,
        payment: &PaymentData,
        tx_index: i32,
    ) -> RepositoryResult<()> {
        let key = (payment.chain_id.clone(), payment.tx_hash.clone(), tx_index);
        let mut index = self.payment_tx_index.write().unwrap();
        let mut payments = self.payments.write().unwrap();

        if let Some(existing_id) = index.get(&key).copied()
            && let Some(existing) = payments.get_mut(&existing_id)
        {
            // Mirrors the `ON CONFLICT ... DO UPDATE SET` list in
            // `postgres::payment::upsert_payment_row`.
            if payment.block_number.is_some() {
                existing.block_number = payment.block_number;
            }
            if payment.confirmed_at.is_some() {
                existing.confirmed_at = payment.confirmed_at;
            }
            if payment.extra.is_some() {
                existing.extra = payment.extra.clone();
            }
            if payment.credited_amount.is_some() {
                existing.credited_amount = payment.credited_amount.clone();
            }
            if payment.rate_used.is_some() {
                existing.rate_used = payment.rate_used.clone();
            }
            if payment.rate_applied_at.is_some() {
                existing.rate_applied_at = payment.rate_applied_at;
            }
        } else {
            index.insert(key, payment.id);
            payments.insert(payment.id, payment.clone());
        }

        Ok(())
    }

    async fn upsert_with_tx_index_and_obligation(
        &self,
        payment: &PaymentData,
        tx_index: i32,
        event_type: &str,
    ) -> RepositoryResult<()> {
        if self.fail_payment_writes.load(Ordering::SeqCst) {
            return Err(RepositoryError::Database(
                "simulated transient payment write failure".to_string(),
            ));
        }
        // Not a real transaction - this double has no rollback to offer -
        // but the two writes below are the same two the Postgres
        // implementation makes atomic, so a test against this double still
        // exercises "both writes happen together" for the payment_handler
        // call site, just not "or neither does".
        self.upsert_with_tx_index(payment, tx_index).await?;

        // A redelivered event mints a fresh `payment.id` every call
        // (`handle_payment_detected` calls `Uuid::new_v4()` unconditionally),
        // so deduping on that id would never match and would requeue a
        // second obligation on every redelivery. The real key is whichever
        // row `(chain_id, tx_hash, tx_index)` resolved to just above -
        // Postgres's `ON CONFLICT ... RETURNING id` returns exactly that
        // existing row's id for the same reason. Looking it back up here is
        // this double's equivalent.
        let key = (payment.chain_id.clone(), payment.tx_hash.clone(), tx_index);
        let resolved_payment_id = *self
            .payment_tx_index
            .read()
            .unwrap()
            .get(&key)
            .unwrap_or(&payment.id);

        let mut outbox = self.webhook_outbox.write().unwrap();
        if !outbox
            .iter()
            .any(|o| o.payment_id == resolved_payment_id && o.event_type == event_type)
        {
            outbox.push(crate::WebhookObligation {
                id: Uuid::new_v4(),
                payment_id: resolved_payment_id,
                invoice_id: payment.invoice_id.as_str().to_string(),
                event_type: event_type.to_string(),
                created_at: Utc::now(),
            });
        }

        Ok(())
    }
}

#[async_trait]
impl crate::reorg::ReorgCandidateReader for InMemoryDataService {
    async fn reorg_candidates(
        &self,
        chain_id: &types::ChainId,
        fork_block: u64,
    ) -> RepositoryResult<Vec<PaymentData>> {
        let payments = self.payments.read().unwrap();
        Ok(payments
            .values()
            .filter(|p| {
                &p.chain_id == chain_id
                    && p.block_number.is_some_and(|b| b >= fork_block)
                    && !p.reorged
            })
            .cloned()
            .collect())
    }
}

#[async_trait]
impl crate::reorg::ReorgWriter for InMemoryDataService {
    async fn mark_payment_reorged(&self, id: Uuid) -> RepositoryResult<()> {
        let mut payments = self.payments.write().unwrap();
        if let Some(payment) = payments.get_mut(&id) {
            payment.reorged = true;
            payment.confirmed_at = None;
        }
        Ok(())
    }
}

#[async_trait]
impl ChainCursorReader for InMemoryDataService {
    async fn chain_cursors(&self, adapter_id: &str) -> RepositoryResult<HashMap<u64, ChainCursor>> {
        if self.fail_chain_cursors.load(Ordering::SeqCst) {
            return Err(RepositoryError::Database(
                "simulated chain_cursors failure".to_string(),
            ));
        }
        let cursors = self.chain_cursors.read().unwrap();
        Ok(cursors
            .iter()
            .filter(|((adapter, _), _)| adapter == adapter_id)
            .map(|((_, chain_id), cursor)| (*chain_id, *cursor))
            .collect())
    }
}

#[async_trait]
impl ChainCursorWriter for InMemoryDataService {
    async fn commit_chain_cursor(
        &self,
        adapter_id: &str,
        chain_id: u64,
        cursor: ChainCursor,
    ) -> RepositoryResult<()> {
        if self.fail_commit_chain_cursor.load(Ordering::SeqCst) {
            return Err(RepositoryError::Database(
                "simulated commit_chain_cursor failure".to_string(),
            ));
        }
        // Same guard as the Postgres upsert: within one epoch a cursor
        // never moves backwards.
        let mut cursors = self.chain_cursors.write().unwrap();
        let key = (adapter_id.to_string(), chain_id);
        let advances = cursors
            .get(&key)
            .is_none_or(|old| old.epoch != cursor.epoch || old.seq < cursor.seq);
        if advances {
            cursors.insert(key, cursor);
        }
        Ok(())
    }

    async fn delete_chain_cursor(&self, adapter_id: &str, chain_id: u64) -> RepositoryResult<()> {
        if self.fail_delete_chain_cursor.load(Ordering::SeqCst) {
            return Err(RepositoryError::Database(
                "simulated delete_chain_cursor failure".to_string(),
            ));
        }
        self.chain_cursors
            .write()
            .unwrap()
            .remove(&(adapter_id.to_string(), chain_id));
        Ok(())
    }

    async fn reset_chain_watch_notifications(&self, _chain_id: u64) -> RepositoryResult<u64> {
        if self
            .fail_reset_chain_watch_notifications
            .load(Ordering::SeqCst)
        {
            return Err(RepositoryError::Database(
                "simulated reset_chain_watch_notifications failure".to_string(),
            ));
        }
        // `WatchedAddressWriter::mark_notified` is already a no-op above:
        // this test double does not model `monitor_notified` at all. The
        // call still counts, so a test can assert this was reached without
        // needing to model the column it would flip.
        self.watch_reset_calls.fetch_add(1, Ordering::SeqCst);
        Ok(0)
    }
}

#[async_trait]
impl PaymentOptionReader for InMemoryDataService {
    async fn get(&self, id: &PaymentOptionId) -> RepositoryResult<Option<PaymentOptionData>> {
        let options = self.payment_options.read().unwrap();
        Ok(options.get(&id.0).cloned())
    }

    async fn get_for_invoice(
        &self,
        invoice_id: &InvoiceId,
    ) -> RepositoryResult<Vec<PaymentOptionData>> {
        let options = self.payment_options.read().unwrap();
        Ok(options
            .values()
            .filter(|po| po.invoice_id == *invoice_id)
            .cloned()
            .collect())
    }

    async fn get_by_payment_method(
        &self,
        invoice_id: &InvoiceId,
        payment_method_id: &PaymentMethodId,
    ) -> RepositoryResult<Option<PaymentOptionData>> {
        let options = self.payment_options.read().unwrap();
        Ok(options
            .values()
            .find(|po| po.invoice_id == *invoice_id && po.payment_method_id == *payment_method_id)
            .cloned())
    }

    async fn get_active_for_invoice(
        &self,
        invoice_id: &InvoiceId,
    ) -> RepositoryResult<Vec<PaymentOptionData>> {
        let options = self.payment_options.read().unwrap();
        Ok(options
            .values()
            .filter(|po| po.invoice_id == *invoice_id && po.is_active)
            .cloned()
            .collect())
    }

    async fn get_by_address(
        &self,
        address: &str,
        chain_id: &types::ChainId,
        token_address: Option<&str>,
    ) -> RepositoryResult<Option<PaymentOptionData>> {
        let options = self.payment_options.read().unwrap();
        Ok(options
            .values()
            .find(|po| {
                po.payment_address == address
                    && &po.chain_id == chain_id
                    && po.token_address.as_deref() == token_address
            })
            .cloned())
    }
}

#[async_trait]
impl PaymentOptionWriter for InMemoryDataService {
    async fn create(&self, option: &PaymentOptionData) -> RepositoryResult<()> {
        let mut options = self.payment_options.write().unwrap();
        options.insert(option.id.0, option.clone());
        Ok(())
    }

    async fn update(&self, option: &PaymentOptionData) -> RepositoryResult<()> {
        let mut options = self.payment_options.write().unwrap();
        if options.contains_key(&option.id.0) {
            options.insert(option.id.0, option.clone());
        }
        Ok(())
    }

    async fn deactivate(&self, id: &PaymentOptionId) -> RepositoryResult<bool> {
        let mut options = self.payment_options.write().unwrap();
        if let Some(po) = options.get_mut(&id.0)
            && po.is_active
        {
            po.is_active = false;
            return Ok(true);
        }
        Ok(false)
    }

    async fn deactivate_for_invoice(&self, invoice_id: &InvoiceId) -> RepositoryResult<u64> {
        let mut options = self.payment_options.write().unwrap();
        let mut count = 0u64;
        for po in options.values_mut() {
            if po.invoice_id == *invoice_id && po.is_active {
                po.is_active = false;
                count += 1;
            }
        }
        Ok(count)
    }
}

#[async_trait]
impl WatchedAddressReader for InMemoryDataService {
    async fn get_invoice_id(
        &self,
        address: &str,
        chain_id: &types::ChainId,
        token_address: Option<&str>,
    ) -> RepositoryResult<Option<InvoiceId>> {
        let addresses = self.addresses.read().unwrap();
        let options = self.payment_options.read().unwrap();

        if let Some(po_id) = addresses.get(&(
            address.to_string(),
            chain_id.clone(),
            token_address.map(String::from),
        )) && let Some(po) = options.get(&po_id.0)
        {
            return Ok(Some(po.invoice_id.clone()));
        }
        Ok(None)
    }

    async fn get_payment_option_id(
        &self,
        address: &str,
        chain_id: &types::ChainId,
        token_address: Option<&str>,
    ) -> RepositoryResult<Option<PaymentOptionId>> {
        let addresses = self.addresses.read().unwrap();
        Ok(addresses
            .get(&(
                address.to_string(),
                chain_id.clone(),
                token_address.map(String::from),
            ))
            .cloned())
    }

    async fn get_pending(&self) -> RepositoryResult<Vec<PendingWatchInfo>> {
        let addresses = self.addresses.read().unwrap();
        let options = self.payment_options.read().unwrap();

        Ok(addresses
            .iter()
            .filter_map(|((addr, chain_id, token_address), po_id)| {
                options.get(&po_id.0).map(|po| PendingWatchInfo {
                    address: addr.clone(),
                    payment_option_id: po_id.clone(),
                    invoice_id: po.invoice_id.as_str().to_string(),
                    chain_id: chain_id.clone(),
                    expected_amount: Some(po.amount.clone()),
                    token_address: token_address.clone(),
                })
            })
            .collect())
    }

    async fn get_expired_for_cleanup(
        &self,
        _grace_period_secs: i64,
    ) -> RepositoryResult<Vec<CleanupAddressInfo>> {
        let addresses = self.addresses.read().unwrap();
        let options = self.payment_options.read().unwrap();
        let invoices = self.invoices.read().unwrap();
        let now = Utc::now();

        Ok(addresses
            .iter()
            .filter_map(|((addr, chain_id, token_address), po_id)| {
                options.get(&po_id.0).and_then(|po| {
                    invoices.get(&po.invoice_id.0).and_then(|inv| {
                        if inv.status == InvoiceStatus::Expired && inv.expires_at < now {
                            Some(CleanupAddressInfo {
                                address: addr.clone(),
                                payment_option_id: po_id.clone(),
                                invoice_id: po.invoice_id.as_str().to_string(),
                                chain_id: chain_id.clone(),
                                token_address: token_address.clone(),
                            })
                        } else {
                            None
                        }
                    })
                })
            })
            .collect())
    }

    async fn get_paid_for_cleanup(&self) -> RepositoryResult<Vec<CleanupAddressInfo>> {
        let addresses = self.addresses.read().unwrap();
        let options = self.payment_options.read().unwrap();
        let invoices = self.invoices.read().unwrap();

        Ok(addresses
            .iter()
            .filter_map(|((addr, chain_id, token_address), po_id)| {
                options.get(&po_id.0).and_then(|po| {
                    invoices.get(&po.invoice_id.0).and_then(|inv| {
                        if inv.status == InvoiceStatus::Paid {
                            Some(CleanupAddressInfo {
                                address: addr.clone(),
                                payment_option_id: po_id.clone(),
                                invoice_id: po.invoice_id.as_str().to_string(),
                                chain_id: chain_id.clone(),
                                token_address: token_address.clone(),
                            })
                        } else {
                            None
                        }
                    })
                })
            })
            .collect())
    }

    async fn get_cancelled_for_cleanup(&self) -> RepositoryResult<Vec<CleanupAddressInfo>> {
        let addresses = self.addresses.read().unwrap();
        let options = self.payment_options.read().unwrap();
        let invoices = self.invoices.read().unwrap();

        Ok(addresses
            .iter()
            .filter_map(|((addr, chain_id, token_address), po_id)| {
                options.get(&po_id.0).and_then(|po| {
                    invoices.get(&po.invoice_id.0).and_then(|inv| {
                        if inv.status == InvoiceStatus::Cancelled {
                            Some(CleanupAddressInfo {
                                address: addr.clone(),
                                payment_option_id: po_id.clone(),
                                invoice_id: po.invoice_id.as_str().to_string(),
                                chain_id: chain_id.clone(),
                                token_address: token_address.clone(),
                            })
                        } else {
                            None
                        }
                    })
                })
            })
            .collect())
    }
}

#[async_trait]
impl WatchedAddressWriter for InMemoryDataService {
    async fn upsert(
        &self,
        address: &str,
        payment_option_id: &PaymentOptionId,
        chain_id: &types::ChainId,
        token_address: Option<&str>,
    ) -> RepositoryResult<()> {
        let mut addresses = self.addresses.write().unwrap();
        addresses.insert(
            (
                address.to_string(),
                chain_id.clone(),
                token_address.map(String::from),
            ),
            payment_option_id.clone(),
        );
        Ok(())
    }

    async fn mark_notified(
        &self,
        _address: &str,
        _chain_id: &ChainId,
        _token_address: Option<&str>,
    ) -> RepositoryResult<()> {
        // No-op for in-memory testing
        Ok(())
    }

    async fn deactivate(
        &self,
        address: &str,
        chain_id: &types::ChainId,
        token_address: Option<&str>,
    ) -> RepositoryResult<bool> {
        let mut addresses = self.addresses.write().unwrap();
        let key = (
            address.to_string(),
            chain_id.clone(),
            token_address.map(String::from),
        );
        if addresses.remove(&key).is_some() {
            Ok(true)
        } else {
            Ok(false)
        }
    }

    async fn deactivate_for_payment_option(
        &self,
        payment_option_id: &PaymentOptionId,
    ) -> RepositoryResult<u64> {
        let mut addresses = self.addresses.write().unwrap();
        let keys_to_remove: Vec<_> = addresses
            .iter()
            .filter(|(_, po_id)| **po_id == *payment_option_id)
            .map(|(k, _)| k.clone())
            .collect();
        let count = keys_to_remove.len() as u64;
        for key in keys_to_remove {
            addresses.remove(&key);
        }
        Ok(count)
    }
}

#[async_trait]
impl TokenReader for InMemoryDataService {
    async fn get(&self, id: i64) -> RepositoryResult<Option<TokenData>> {
        let tokens = self.tokens.read().unwrap();
        Ok(tokens.get(&id).cloned())
    }

    async fn get_by_address(
        &self,
        chain_id: &ChainId,
        address: &str,
    ) -> RepositoryResult<Option<TokenData>> {
        let tokens = self.tokens.read().unwrap();
        Ok(tokens
            .values()
            .find(|t| &t.chain_id == chain_id && t.address.eq_ignore_ascii_case(address))
            .cloned())
    }

    async fn find_by_symbol(
        &self,
        chain_id: &ChainId,
        symbol: &str,
    ) -> RepositoryResult<Option<TokenData>> {
        let tokens = self.tokens.read().unwrap();
        Ok(tokens
            .values()
            .find(|t| {
                &t.chain_id == chain_id
                    && t.symbol
                        .as_ref()
                        .is_some_and(|s| s.eq_ignore_ascii_case(symbol))
            })
            .cloned())
    }

    async fn query(&self, params: &TokenQueryParams) -> RepositoryResult<(i64, Vec<TokenData>)> {
        let tokens = self.tokens.read().unwrap();
        let mut results: Vec<TokenData> = tokens
            .values()
            .filter(|t| {
                if let Some(ref token_type) = params.token_type
                    && t.token_type != *token_type
                {
                    return false;
                }
                if let Some(ref chain) = params.chain_id
                    && t.chain_id != *chain
                {
                    return false;
                }
                if let Some(enabled) = params.enabled
                    && t.enabled != enabled
                {
                    return false;
                }
                if let Some(ref symbol) = params.symbol
                    && !t
                        .symbol
                        .as_ref()
                        .is_some_and(|s| s.eq_ignore_ascii_case(symbol))
                {
                    return false;
                }
                true
            })
            .cloned()
            .collect();

        let total = results.len() as i64;
        results.sort_by(|a, b| {
            // Sorted by identifier, not display name: there is no display name
            // without a chain_configs lookup, and the identifier is stable.
            a.chain_id
                .cmp(&b.chain_id)
                .then_with(|| a.symbol.cmp(&b.symbol))
        });

        let offset = params.offset as usize;
        let limit = params.limit as usize;
        let results = results.into_iter().skip(offset).take(limit).collect();

        Ok((total, results))
    }

    async fn get_enabled_for_chain(&self, chain_id: &ChainId) -> RepositoryResult<Vec<TokenData>> {
        let tokens = self.tokens.read().unwrap();
        Ok(tokens
            .values()
            .filter(|t| &t.chain_id == chain_id && t.enabled)
            .cloned()
            .collect())
    }
}

#[async_trait]
impl TokenWriter for InMemoryDataService {
    async fn insert(&self, token: &TokenData) -> RepositoryResult<i64> {
        let mut tokens = self.tokens.write().unwrap();
        let mut counter = self.token_id_counter.write().unwrap();
        *counter += 1;
        let id = *counter;

        let mut token = token.clone();
        token.id = Some(id);
        tokens.insert(id, token);

        Ok(id)
    }

    async fn update(&self, token: &TokenData) -> RepositoryResult<()> {
        let id = token.id.ok_or_else(|| {
            types::RepositoryError::InvalidData("Token must have an ID for update".into())
        })?;

        let mut tokens = self.tokens.write().unwrap();
        if let std::collections::hash_map::Entry::Occupied(mut e) = tokens.entry(id) {
            e.insert(token.clone());
            Ok(())
        } else {
            Err(types::RepositoryError::NotFound(format!(
                "Token {} not found",
                id
            )))
        }
    }

    async fn delete(&self, id: i64) -> RepositoryResult<()> {
        let mut tokens = self.tokens.write().unwrap();
        if tokens.remove(&id).is_some() {
            Ok(())
        } else {
            Err(types::RepositoryError::NotFound(format!(
                "Token {} not found",
                id
            )))
        }
    }

    async fn set_enabled(&self, id: i64, enabled: bool) -> RepositoryResult<()> {
        let mut tokens = self.tokens.write().unwrap();
        if let Some(token) = tokens.get_mut(&id) {
            token.enabled = enabled;
            Ok(())
        } else {
            Err(types::RepositoryError::NotFound(format!(
                "Token {} not found",
                id
            )))
        }
    }
}

#[async_trait]
impl StoreWebhookReader for InMemoryDataService {
    async fn get_webhook(&self, store_id: Uuid) -> RepositoryResult<Option<StoreWebhook>> {
        let webhooks = self.webhooks.read().unwrap();
        Ok(webhooks.get(&store_id).cloned())
    }

    async fn get_enabled_webhook(&self, store_id: Uuid) -> RepositoryResult<Option<StoreWebhook>> {
        let webhooks = self.webhooks.read().unwrap();
        Ok(webhooks.get(&store_id).filter(|w| w.enabled).cloned())
    }
}

#[async_trait]
impl StoreSettingsReader for InMemoryDataService {
    async fn get_store_settings(&self, _store_id: Uuid) -> RepositoryResult<Option<StoreSettings>> {
        Ok(None)
    }
}

#[async_trait]
impl auth::StoreRepository for InMemoryDataService {
    async fn create_store(&self, _store: &types::Store) -> auth::error::Result<()> {
        Ok(())
    }
    async fn get_store(&self, _id: types::StoreId) -> auth::error::Result<Option<types::Store>> {
        Ok(None)
    }
    async fn get_stores_for_user(
        &self,
        _user_id: types::UserId,
    ) -> auth::error::Result<Vec<types::Store>> {
        Ok(vec![])
    }
    async fn get_stores_owned_by(
        &self,
        _user_id: types::UserId,
    ) -> auth::error::Result<Vec<types::Store>> {
        Ok(vec![])
    }
    async fn update_store(&self, _store: &types::Store) -> auth::error::Result<()> {
        Ok(())
    }
    async fn archive_store(&self, _id: types::StoreId) -> auth::error::Result<()> {
        Ok(())
    }
    async fn delete_store(&self, _id: types::StoreId) -> auth::error::Result<()> {
        Ok(())
    }
}

#[async_trait]
impl PaymentEventWriter for InMemoryDataService {
    async fn create_event(
        &self,
        _invoice_id: &InvoiceId,
        _payment_id: Option<Uuid>,
        _event_type: &str,
        _event_data: Option<serde_json::Value>,
    ) -> RepositoryResult<Uuid> {
        // No-op for tests - just return a new UUID
        Ok(Uuid::new_v4())
    }
}

/// No-op, matching `PaymentEventWriter` above: `EventConsumer`'s tests need
/// something that satisfies `WebhookDataService`, not a double that records
/// what was written and lets a test read it back.
#[async_trait]
impl WebhookDeliveryWriter for InMemoryDataService {
    async fn upsert_delivery(&self, _params: UpsertDeliveryParams) -> RepositoryResult<()> {
        Ok(())
    }
}

#[async_trait]
impl crate::WebhookOutboxReader for InMemoryDataService {
    async fn claim_undispatched_obligations(
        &self,
        limit: i64,
        visibility_secs: i64,
    ) -> RepositoryResult<Vec<crate::WebhookObligation>> {
        let now = Utc::now();
        let outbox = self.webhook_outbox.read().unwrap();
        let mut claims = self.webhook_outbox_claims.write().unwrap();

        let mut claimable: Vec<_> = outbox
            .iter()
            .filter(|o| claims.get(&o.id).is_none_or(|until| *until < now))
            .cloned()
            .collect();
        claimable.sort_by_key(|o| o.created_at);
        claimable.truncate(limit.max(0) as usize);

        let claimed_until = now + chrono::Duration::seconds(visibility_secs);
        for obligation in &claimable {
            claims.insert(obligation.id, claimed_until);
        }

        Ok(claimable)
    }
}

#[async_trait]
impl crate::WebhookOutboxWriter for InMemoryDataService {
    async fn mark_obligation_dispatched(&self, id: Uuid) -> RepositoryResult<()> {
        let mut outbox = self.webhook_outbox.write().unwrap();
        outbox.retain(|o| o.id != id);
        self.webhook_outbox_claims.write().unwrap().remove(&id);
        Ok(())
    }
}

/// Create a test invoice (network-agnostic).
pub fn create_test_invoice() -> InvoiceData {
    InvoiceData {
        id: InvoiceId::new(),
        store_id: StoreId::new(),
        currency: "USD".to_string(),
        status: InvoiceStatus::Pending,
        amount: "100.00".to_string(), // $100 USD
        amount_received: "0".to_string(),
        created_at: Utc::now(),
        expires_at: Utc::now() + chrono::Duration::hours(1),
        metadata: None,
        customer_email: None,
        extra: None,
    }
}

/// Create a test payment option for an invoice.
pub fn create_test_payment_option(invoice_id: &InvoiceId) -> PaymentOptionData {
    PaymentOptionData {
        id: PaymentOptionId(Uuid::new_v4()),
        invoice_id: invoice_id.clone(),
        payment_method_id: PaymentMethodId::new("ETH", &ChainId::evm(1)),
        chain_id: ChainId::evm(1),
        asset_symbol: "ETH".to_string(),
        token_address: None,
        decimals: 18,
        payment_address: "0x1234567890abcdef1234567890abcdef12345678".to_string(),
        wallet_id: None,
        derivation_index: None,
        amount: "50000000000000000".to_string(), // ~0.05 ETH worth $100 at hypothetical rate
        rate: Some("2000.00".to_string()),
        rate_at: Some(Utc::now()),
        is_active: true,
        created_at: Utc::now(),
    }
}

/// Create a test payment.
pub fn create_test_payment(
    invoice_id: &InvoiceId,
    payment_option_id: Option<&PaymentOptionId>,
) -> PaymentData {
    PaymentData {
        id: Uuid::new_v4(),
        invoice_id: invoice_id.clone(),
        payment_option_id: payment_option_id.map(|po| po.0),
        chain_id: ChainId::evm(1), // Ethereum mainnet
        asset_type: types::AssetType::Native,
        amount: "50000000000000000".to_string(),
        asset_symbol: "ETH".to_string(),
        token_address: None,
        tx_hash: "0xabcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890".to_string(),
        block_number: Some(12345678),
        detected_at: Utc::now(),
        confirmed_at: None,
        from_address: Some("0xabcdef1234567890abcdef1234567890abcdef12".to_string()),
        reorged: false,
        extra: None,
        credited_amount: Some("0.05".to_string()), // 0.05 ETH
        rate_used: None,
        rate_applied_at: None,
    }
}

// =============================================================================
// Payment Analytics
// =============================================================================

/// Mirrors the Postgres `payment_volume_by_day` query.
///
/// Every rule the SQL enforces is restated here on purpose — empty store list
/// matches nothing, reorged payments are excluded, the window is
/// `[since, until)`, decimals fall back to 18 when the payment option is gone,
/// and `decimals` is part of the group key. A double that quietly disagrees
/// with the real store about one of those is the failure mode this guards.
#[async_trait]
impl PaymentAnalyticsReader for InMemoryDataService {
    async fn payment_volume_by_day(
        &self,
        query: &PaymentVolumeQuery,
    ) -> RepositoryResult<Vec<PaymentVolumeBucket>> {
        if query.store_ids.is_empty() {
            return Ok(Vec::new());
        }

        let payments = self.payments.read().unwrap();
        let invoices = self.invoices.read().unwrap();
        let options = self.payment_options.read().unwrap();

        // Key: (day, asset_symbol, decimals) -> (raw sum, count)
        let mut groups: BTreeMap<(NaiveDate, String, u8), (u128, i64)> = BTreeMap::new();

        for payment in payments.values() {
            if payment.reorged {
                continue;
            }
            if payment.detected_at < query.since || payment.detected_at >= query.until {
                continue;
            }
            let Some(invoice) = invoices.get(&payment.invoice_id.0) else {
                continue;
            };
            if !query.store_ids.contains(&invoice.store_id) {
                continue;
            }

            let decimals = payment
                .payment_option_id
                .and_then(|id| options.get(&id))
                .map_or(18, |po| po.decimals);

            let amount: u128 = payment.amount.parse().map_err(|_| {
                RepositoryError::Database(format!(
                    "payment {} has a non-integer amount: {}",
                    payment.id, payment.amount
                ))
            })?;

            let entry = groups
                .entry((
                    payment.detected_at.date_naive(),
                    payment.asset_symbol.clone(),
                    decimals,
                ))
                .or_insert((0, 0));
            entry.0 = entry.0.saturating_add(amount);
            entry.1 += 1;
        }

        Ok(groups
            .into_iter()
            .map(
                |((day, asset_symbol, decimals), (raw, count))| PaymentVolumeBucket {
                    day,
                    asset_symbol,
                    decimals,
                    raw_amount: raw.to_string(),
                    payment_count: count,
                },
            )
            .collect())
    }

    async fn payment_volume_by_day_per_store(
        &self,
        query: &PaymentVolumeQuery,
    ) -> RepositoryResult<Vec<StorePaymentVolumeBucket>> {
        if query.store_ids.is_empty() {
            return Ok(Vec::new());
        }

        let payments = self.payments.read().unwrap();
        let invoices = self.invoices.read().unwrap();
        let options = self.payment_options.read().unwrap();

        // Key: (store_id, day, asset_symbol, decimals) -> (raw sum, count).
        // Identical to `payment_volume_by_day` with the store carried into
        // the group key rather than only the filter. Keyed on the inner
        // `Uuid` rather than `StoreId` itself, which does not derive `Ord`.
        let mut groups: BTreeMap<(Uuid, NaiveDate, String, u8), (u128, i64)> = BTreeMap::new();

        for payment in payments.values() {
            if payment.reorged {
                continue;
            }
            if payment.detected_at < query.since || payment.detected_at >= query.until {
                continue;
            }
            let Some(invoice) = invoices.get(&payment.invoice_id.0) else {
                continue;
            };
            if !query.store_ids.contains(&invoice.store_id) {
                continue;
            }

            let decimals = payment
                .payment_option_id
                .and_then(|id| options.get(&id))
                .map_or(18, |po| po.decimals);

            let amount: u128 = payment.amount.parse().map_err(|_| {
                RepositoryError::Database(format!(
                    "payment {} has a non-integer amount: {}",
                    payment.id, payment.amount
                ))
            })?;

            let entry = groups
                .entry((
                    invoice.store_id.0,
                    payment.detected_at.date_naive(),
                    payment.asset_symbol.clone(),
                    decimals,
                ))
                .or_insert((0, 0));
            entry.0 = entry.0.saturating_add(amount);
            entry.1 += 1;
        }

        Ok(groups
            .into_iter()
            .map(|((store_id, day, asset_symbol, decimals), (raw, count))| {
                StorePaymentVolumeBucket {
                    store_id: StoreId(store_id),
                    day,
                    asset_symbol,
                    decimals,
                    raw_amount: raw.to_string(),
                    payment_count: count,
                }
            })
            .collect())
    }
}
