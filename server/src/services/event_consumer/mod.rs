//! Event consumer service for processing monitor events.
//!
//! Subscribes to evmmonitor events via the EventBridge and updates
//! invoice/payment state in the database.

mod confirmation_handler;
mod payment_handler;
mod reorg_handler;
mod webhook_dispatch;

#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use auth::StoreRepository;
use bigdecimal::{BigDecimal, RoundingMode, Zero};
use data_service::{
    ChainCursor, ChainCursorReader, ChainCursorWriter, PaymentOptionReader, PaymentTxIndexReader,
    PaymentTxIndexWriter, ReorgCandidateReader, ReorgWriter, SettlementToleranceReader,
    SettlementToleranceWriter,
};
use evm::EvmError;
use evm::monitor::bridge::{EventBridge, EventCursor, EventEnvelope};
use evm::monitor::events::MonitorEvent;
use tokio_stream::StreamExt;
use types::{
    InvoiceReader, InvoiceWriter, PaymentReader, PaymentWriter, StoreSettingsReader, TokenReader,
    WatchedAddressReader,
};

use super::email::EmailSender;
use super::evm_monitor::EVMMonitor;
use super::invoice_cleanup::{CleanupDataService, InvoiceCleanupService};
use super::plugins::OwnStorePaymentObserver;
use super::webhook::{WebhookDataService, WebhookSink};
use crate::api::ws::WsBroadcast;

/// Identifies this server's event source when persisting resume cursors.
///
/// A plain constant because there is exactly one adapter kind today
/// (evmmonitor); the column exists so a second adapter would not collide
/// with it, not because this server picks between several.
const ADAPTER_ID: &str = "evmmonitor";

/// Trait for data service requirements in EventConsumer.
pub trait EventConsumerDataService:
    InvoiceReader
    + InvoiceWriter
    + PaymentReader
    + PaymentWriter
    + PaymentTxIndexWriter
    + PaymentTxIndexReader
    + SettlementToleranceReader
    + SettlementToleranceWriter
    + PaymentOptionReader
    + TokenReader
    + WatchedAddressReader
    + StoreSettingsReader
    + StoreRepository
    + CleanupDataService
    + ReorgCandidateReader
    + ReorgWriter
    + ChainCursorReader
    + ChainCursorWriter
    + Send
    + Sync
{
}

impl<T> EventConsumerDataService for T where
    T: InvoiceReader
        + InvoiceWriter
        + PaymentReader
        + PaymentWriter
        + PaymentTxIndexWriter
        + PaymentTxIndexReader
        + SettlementToleranceReader
        + SettlementToleranceWriter
        + PaymentOptionReader
        + TokenReader
        + WatchedAddressReader
        + StoreSettingsReader
        + StoreRepository
        + CleanupDataService
        + ReorgCandidateReader
        + ReorgWriter
        + ChainCursorReader
        + ChainCursorWriter
        + Send
        + Sync
{
}

/// What to do when an envelope fails to apply. Takes the chain and the
/// outbox position that failed.
///
/// Production leaves this `None`, which logs and exits the process so
/// Docker restarts it: there is no in-process fix for a `handle_event`
/// error that lets the cursor stay honest, since the loop cannot un-fail
/// the DB write and must not let a *later*, successful envelope on the same
/// chain commit a cursor past this one - that would make the failed
/// envelope unresumable forever, exactly the loss this whole mechanism
/// exists to close. Tests set this to observe that the failure was reached
/// without killing the test binary via `process::exit`.
pub type ApplyFailureHook = Arc<dyn Fn(u64, i64) + Send + Sync>;

/// What to do when the consumer cannot safely continue at all - a startup
/// step failed before a single envelope was ever read, a lineage break could
/// not re-arm `watch_retry`, or the event stream itself ended for a reason
/// other than the already-handled apply-failure path.
///
/// Production leaves this `None`, which logs and exits the process, same as
/// [`ApplyFailureHook`]: none of these paths have an in-process retry that
/// would do anything but spin against the same failing dependency, and
/// returning quietly would leave a healthy-looking process with no consumer
/// running at all - indistinguishable from an idle queue. Tests set this to
/// observe the failure without killing the test binary.
pub type ResumeFailureHook = Arc<dyn Fn(&str) + Send + Sync>;

/// Event consumer that processes monitor events and updates database state.
///
/// Optionally sends webhook notifications when invoice status changes.
pub struct EventConsumer<D: EventConsumerDataService, M: EVMMonitor, W: WebhookDataService = D> {
    bridge: Arc<dyn EventBridge>,
    data_service: Arc<D>,
    cleanup_service: Option<Arc<InvoiceCleanupService<D, M, W>>>,
    webhook_service: Option<Arc<dyn WebhookSink>>,
    ws_broadcast: Option<Arc<WsBroadcast>>,
    email_sender: Arc<dyn EmailSender>,
    /// Capability 4 observers, and the one store they may hear about.
    ///
    /// Both default to "nothing": an instance with no configured operator
    /// store has no observers either, and
    /// `own_store_id: None` reports nothing even if an observer is somehow
    /// registered. See `plugins::payment_observer`.
    payment_observers: Vec<Arc<dyn OwnStorePaymentObserver>>,
    own_store_id: Option<types::StoreId>,
    on_apply_failure: Option<ApplyFailureHook>,
    on_resume_failure: Option<ResumeFailureHook>,
    accept_lineage_break: bool,
    skip_events: HashSet<(u64, i64, i64)>,
}

impl<
    D: EventConsumerDataService + 'static,
    M: EVMMonitor + 'static,
    W: WebhookDataService + 'static,
> EventConsumer<D, M, W>
{
    /// Create a new event consumer with optional services.
    pub fn new(
        bridge: Arc<dyn EventBridge>,
        data_service: Arc<D>,
        cleanup_service: Option<Arc<InvoiceCleanupService<D, M, W>>>,
        webhook_service: Option<Arc<dyn WebhookSink>>,
        ws_broadcast: Option<Arc<WsBroadcast>>,
        email_sender: Arc<dyn EmailSender>,
    ) -> Self {
        Self {
            bridge,
            data_service,
            cleanup_service,
            webhook_service,
            ws_broadcast,
            email_sender,
            payment_observers: Vec::new(),
            own_store_id: None,
            on_apply_failure: None,
            on_resume_failure: None,
            accept_lineage_break: false,
            skip_events: HashSet::new(),
        }
    }

    /// Let the consumer resume across an outbox lineage break instead of
    /// refusing to start.
    ///
    /// A stored cursor whose epoch differs from the outbox's means events
    /// between the last commit and now may be gone, and there is no rescan by
    /// `block_height` to recover them. Re-arming `watch_retry` only re-drives
    /// watches that are still active, so a payment that confirmed and whose
    /// watch expired inside the gap would never be credited. By default the
    /// consumer therefore halts, so a human audits the gap; an operator who
    /// has done that opts in here, and the break is then handled by
    /// re-arming `watch_retry` and forgetting the stale cursors.
    #[must_use]
    pub fn with_accepted_lineage_break(mut self, accept: bool) -> Self {
        self.accept_lineage_break = accept;
        self
    }

    /// Skip the named `(chain_id, seq)` envelopes instead of applying them.
    ///
    /// The escape hatch for a poison event: one that can never apply halts
    /// the consumer on every restart, and with it every merchant's crediting.
    /// The operator names the exact envelope from the halt log after deciding
    /// what to do with the deposit by hand; nothing is skipped unless listed,
    /// so the default is still "never advance past an unapplied payment".
    #[must_use]
    pub fn with_skipped_events(mut self, skip: impl IntoIterator<Item = (u64, i64, i64)>) -> Self {
        self.skip_events = skip.into_iter().collect();
        self
    }

    /// Report settled invoices on `own_store_id` to `observers`.
    ///
    /// Separate from `new` rather than two more positional arguments: every
    /// existing caller wants neither, and a call site that silently passed
    /// the wrong store here would leak merchants' payments to a plugin. An
    /// instance that never calls this reports nothing, which is the state
    /// every deployment is in until a billing plugin is configured.
    #[must_use]
    pub fn with_own_store_payments(
        mut self,
        own_store_id: types::StoreId,
        observers: Vec<Arc<dyn OwnStorePaymentObserver>>,
    ) -> Self {
        self.own_store_id = Some(own_store_id);
        self.payment_observers = observers;
        self
    }

    /// Override what happens when an envelope fails to apply.
    ///
    /// Only tests should call this - see [`ApplyFailureHook`] for why
    /// production wants the default `process::exit(1)`.
    #[must_use]
    pub fn with_apply_failure_hook(mut self, hook: ApplyFailureHook) -> Self {
        self.on_apply_failure = Some(hook);
        self
    }

    /// Override what happens when the consumer cannot safely continue.
    ///
    /// Only tests should call this - see [`ResumeFailureHook`] for why
    /// production wants the default `process::exit(1)`.
    #[must_use]
    pub fn with_resume_failure_hook(mut self, hook: ResumeFailureHook) -> Self {
        self.on_resume_failure = Some(hook);
        self
    }

    /// Fail loudly and irrecoverably: the caller has already logged what
    /// went wrong, this decides how to react to it. See [`ResumeFailureHook`]
    /// for why production always exits here rather than trying to carry on.
    fn fatal(&self, reason: &str) {
        match &self.on_resume_failure {
            Some(hook) => hook(reason),
            None => std::process::exit(1),
        }
    }

    /// Run the event consumer as a background task.
    ///
    /// This should be spawned with `tokio::spawn(consumer.run())`.
    #[allow(clippy::cognitive_complexity)]
    pub async fn run(self) {
        tracing::info!("Starting event consumer");

        let mut cursors =
            match ChainCursorReader::chain_cursors(&*self.data_service, ADAPTER_ID).await {
                Ok(c) => c,
                Err(e) => {
                    // A failed load is indistinguishable from an empty
                    // `HashMap` to everything downstream, but they are not
                    // the same thing: an empty map means "never resumed
                    // before, nothing to lose," which skips the
                    // epoch-mismatch check below and resumes from whatever
                    // the outbox currently retains. Silently doing that on a
                    // DB hiccup would drop real cursors this server had.
                    // Refusing to start is the safe failure here.
                    tracing::error!(error = %e, "failed to load chain cursors; refusing to start");
                    self.fatal("failed to load chain cursors");
                    return;
                }
            };

        let bridge_epoch = match self.bridge.current_epoch().await {
            Ok(e) => e,
            Err(e) => {
                tracing::error!(error = %e, "failed to read the event outbox's epoch");
                self.fatal("failed to read the event outbox's epoch");
                return;
            }
        };

        let resume_from = match self.reconcile_cursors(&mut cursors, bridge_epoch).await {
            Ok(r) => r,
            // `reconcile_cursors` already logged and called `fatal` for
            // whatever went wrong; in production that already exited the
            // process, so this `return` only matters to a test hook.
            Err(()) => return,
        };

        // An out-of-range resume position means events between the last
        // committed cursor and the oldest retained entry are gone. Carrying
        // on from the oldest retained entry would be exactly the silent
        // "start from now" this mechanism exists to rule out: `watch_retry`
        // only re-drives watches that are still active, so a payment that
        // confirmed in the gap on a since-expired watch would never be
        // credited. Fail loudly instead, so a human is paged.
        let mut event_stream = match self.bridge.subscribe_from(resume_from).await {
            Ok(stream) => stream,
            Err(EvmError::EventStreamOutOfRange(reason)) => {
                tracing::error!(
                    reason = %reason,
                    "resume position no longer retained; events since the last committed \
                     cursor cannot be replayed. Refusing to resume from the oldest retained \
                     entry - audit the gap before restarting."
                );
                self.fatal("event resume position is out of range");
                return;
            }
            Err(e) => {
                tracing::error!(error = %e, "Failed to subscribe to events");
                self.fatal("failed to subscribe to events");
                return;
            }
        };

        let mut apply_failed = false;
        while let Some(envelope) = event_stream.next().await {
            if !self
                .apply_envelope(envelope, bridge_epoch, &mut cursors)
                .await
            {
                // A failed apply already invoked `on_apply_failure` (which
                // exits the process in production). Stopping the loop here
                // too matters for the test hook path, where the override
                // does not exit: without this, the next envelope on this
                // chain could still apply and commit a cursor past the one
                // that just failed.
                apply_failed = true;
                break;
            }
        }

        if apply_failed {
            tracing::warn!("Event stream ended after a failed apply, consumer shutting down");
        } else {
            // The stream itself ended - a dropped connection, a failed
            // `XREAD`, a malformed entry the bridge refused to skip past.
            // Nothing spawned this task with a shutdown signal, so this is
            // never an expected outcome: left as a plain return, the process
            // would keep running with no consumer at all, looking exactly
            // like a healthy, idle one.
            tracing::error!(
                "event stream ended unexpectedly; halting so a restart resumes from the last \
                 committed cursor instead of leaving a dead consumer running silently"
            );
            self.fatal("event stream ended unexpectedly");
        }
    }

    /// Reconcile this server's stored cursors against the outbox's current
    /// epoch, returning where to resume.
    ///
    /// Each chain's cursor is judged on its own epoch, not the map's as a
    /// whole: `apply_envelope` commits `envelope.cursor.epoch` per chain, as
    /// events happen to arrive for it, so after a real lineage break a
    /// busy chain can already be recommitted under the new epoch while an
    /// idle one still carries a stored cursor from before it - both live in
    /// the same `cursors` map at once. Picking one chain's epoch as
    /// representative for all of them (as an earlier version of this did)
    /// blends a stale, pre-break `seq` into the shared low-water mark; since
    /// `seq` numbering can restart after a genuine backing-store loss, that
    /// stale number can coincidentally land *above* where the new lineage
    /// has actually progressed, and resuming from it skips every real event
    /// below it forever, with no error - exactly the failure this whole
    /// mechanism exists to rule out.
    ///
    /// `seq` is one counter for the whole outbox, and this consumer applies
    /// envelopes strictly in `seq` order across every chain, stopping at the
    /// first failure. So the highest `seq` committed by any chain still on
    /// the current epoch is the global applied position: everything at or
    /// below it, on any chain, was applied. Resuming from it is exact.
    /// Resuming from the *lowest* chain cursor instead would let one idle
    /// chain pin the resume point behind the retention window and turn
    /// ordinary traffic on other chains into a false `OUT_OF_RANGE`.
    ///
    /// A chain whose epoch does not match means the outbox lost continuity
    /// for it specifically - `seq` numbers from before the break name a
    /// lineage that may no longer exist. There is no rescan-from-height
    /// fallback, so unless the operator opted in with
    /// [`Self::with_accepted_lineage_break`] this halts. When accepted it does
    /// the next best thing: log
    /// loudly (this is the "page someone" moment, not a silent one),
    /// re-arm `watch_retry` for that chain, and forget its cursor so it
    /// does not pollute the low-water mark next time either. If every chain
    /// mismatches, there is nothing left to resume from and this returns
    /// `None`, same as a cold start.
    ///
    /// `Err(())` means a lineage break could not re-arm `watch_retry` for
    /// every affected chain - see [`Self::break_lineage`] - and the caller
    /// must stop rather than resume with a cursor built on top of an
    /// incomplete break.
    async fn reconcile_cursors(
        &self,
        cursors: &mut HashMap<u64, ChainCursor>,
        bridge_epoch: i64,
    ) -> Result<Option<EventCursor>, ()> {
        if cursors.is_empty() {
            // Never resumed before: nothing stored to lose, so whatever the
            // outbox currently retains from its start is a strict gain.
            return Ok(None);
        }

        let mismatched: Vec<u64> = cursors
            .iter()
            .filter(|(_, c)| c.epoch != bridge_epoch)
            .map(|(&chain_id, _)| chain_id)
            .collect();

        if !mismatched.is_empty() && !self.accept_lineage_break {
            tracing::error!(
                ?mismatched,
                bridge_epoch,
                "event outbox epoch changed since these chains last resumed; events since \
                 their last commit may be lost and cannot be replayed. Refusing to resume - \
                 audit the gap for payments missing from `payments`, then restart with \
                 EVENT_ACCEPT_LINEAGE_BREAK=true to accept the break."
            );
            self.fatal("event outbox lineage break not accepted");
            return Err(());
        }

        if !mismatched.is_empty() {
            tracing::error!(
                ?mismatched,
                bridge_epoch,
                "event outbox epoch changed since these chains last resumed; the gap since \
                 their last commit cannot be replayed. Re-arming watch_retry for every watch \
                 on the affected chains."
            );
            if !self.break_lineage(cursors, &mismatched).await {
                return Err(());
            }
        }

        let max_seq = cursors.values().map(|c| c.seq).max();
        Ok(max_seq.map(|seq| EventCursor {
            epoch: bridge_epoch,
            seq,
            block_height: 0,
        }))
    }

    /// Re-arm `watch_retry` for `chain_ids`, then forget their cursors.
    ///
    /// Used by [`Self::reconcile_cursors`] for chains whose stored epoch no
    /// longer matches the outbox's: the gap since the last commit cannot be
    /// replayed for them, and re-driving every live watch within
    /// `watch_retry`'s normal cycle is the best available recovery. There is
    /// no rescan-from-`block_height` fallback yet.
    ///
    /// `watch_retry` is the *only* safety net for whatever confirmed inside a
    /// gap this replaces - see the module's own notes on the epoch mechanism.
    /// If re-arming it fails for a chain (a DB blip, the same kind of
    /// transient error this path exists to be robust against), there is
    /// nothing left recovering that chain's payments: removing its cursor
    /// anyway would make the break look clean, and keeping the stale cursor
    /// around would let it blend back into the low-water mark next time
    /// (exactly the bug the per-chain epoch check exists to rule out).
    /// Neither is safe, so a chain whose re-arm fails keeps its stale cursor
    /// and this reports failure to the caller, which halts instead of
    /// resuming on top of an incomplete break.
    async fn break_lineage(
        &self,
        cursors: &mut HashMap<u64, ChainCursor>,
        chain_ids: &[u64],
    ) -> bool {
        let mut all_rearmed = true;
        for &chain_id in chain_ids {
            match self
                .data_service
                .reset_chain_watch_notifications(chain_id)
                .await
            {
                Ok(_) => {
                    // Delete the stored row too, not just the in-memory
                    // entry: an idle chain would otherwise keep its old
                    // epoch and read as a fresh mismatch on the next restart,
                    // after the operator has unset the accept flag.
                    if let Err(e) = self
                        .data_service
                        .delete_chain_cursor(ADAPTER_ID, chain_id)
                        .await
                    {
                        tracing::error!(
                            chain_id,
                            error = %e,
                            "failed to delete the stale chain cursor after an event outbox lineage break"
                        );
                        all_rearmed = false;
                        continue;
                    }
                    cursors.remove(&chain_id);
                }
                Err(e) => {
                    tracing::error!(
                        chain_id,
                        error = %e,
                        "failed to re-arm watch_retry after an event outbox lineage break"
                    );
                    all_rearmed = false;
                }
            }
        }
        if !all_rearmed {
            self.fatal("failed to re-arm watch_retry or drop the stale cursor after an event outbox lineage break");
        }
        all_rearmed
    }

    /// Apply one envelope and, only once it has been applied, durably
    /// commit having done so.
    ///
    /// Ordering matters: a crash between "applied" and "committed" is fine,
    /// since delivery is at-least-once and the apply is idempotent, but
    /// committing first and then crashing before applying would silently
    /// lose the event - the exact failure this whole mechanism exists to
    /// close.
    ///
    /// Returns `false` when the caller must stop advancing this stream. Both
    /// a `handle_event` failure and a `commit_chain_cursor` failure are
    /// cases that matter: `cursors` holds one scalar `(epoch, seq)` per
    /// chain, so if the caller kept going and a *later* envelope on the same
    /// chain applied and committed, that commit would move the chain's
    /// cursor past this failed one - on any future resume (restart or
    /// otherwise) resume would start above the failed envelope, treating it as
    /// already applied, and it would never be redelivered. Stopping here
    /// instead means nothing commits past it, so it stays exactly at the
    /// resume point until a retry (in production, a process restart, since
    /// [`ApplyFailureHook`] defaults to exiting) redelivers it.
    async fn apply_envelope(
        &self,
        envelope: EventEnvelope,
        bridge_epoch: i64,
        cursors: &mut HashMap<u64, ChainCursor>,
    ) -> bool {
        let chain_id = envelope.chain_id;

        if self
            .skip_events
            .contains(&(chain_id, envelope.cursor.epoch, envelope.cursor.seq))
        {
            tracing::warn!(
                chain_id,
                epoch = envelope.cursor.epoch,
                seq = envelope.cursor.seq,
                "skipping event named by the operator; it is NOT applied"
            );
        } else if !self.apply_or_halt(&envelope).await {
            return false;
        }

        // Commit the epoch this consumer resumed under, not the one stamped
        // on the entry: after a lineage break the stream can still hold
        // entries from the old epoch, and recording that epoch would make
        // the next restart read a fresh mismatch and halt again, so an
        // accepted break would never converge.
        let cursor = ChainCursor {
            epoch: bridge_epoch,
            seq: envelope.cursor.seq,
            block_height: envelope.cursor.block_height,
        };
        if let Err(e) = self
            .data_service
            .commit_chain_cursor(ADAPTER_ID, chain_id, cursor)
            .await
        {
            // The effect (`handle_event`, above) already applied - idempotent
            // redelivery covers it on restart - but if this keeps failing,
            // `chain_cursors` stops advancing for this chain with nothing
            // else to notice. Halting here, like every other unrecoverable
            // failure in this file, gets that in front of a supervisor
            // instead of a silent log line.
            tracing::error!(
                chain_id,
                seq = envelope.cursor.seq,
                error = %e,
                "failed to commit chain cursor; halting"
            );
            self.halt(chain_id, envelope.cursor.seq);
            return false;
        }
        cursors.insert(chain_id, cursor);
        true
    }

    /// Apply the event. Returns `false` after halting, when the cursor must
    /// not advance.
    ///
    /// Every failure halts, including one that looks permanent (an invoice
    /// since deleted, a malformed token field). Skipping such an event would
    /// commit the cursor past a payment that was never credited, with nothing
    /// pointing at it: a halt is loud, and a human decides what to do with
    /// the deposit.
    async fn apply_or_halt(&self, envelope: &EventEnvelope) -> bool {
        match self.handle_event(envelope.event.clone()).await {
            Ok(()) => true,
            Err(e) => {
                tracing::error!(
                    chain_id = envelope.chain_id,
                    epoch = envelope.cursor.epoch,
                    seq = envelope.cursor.seq,
                    error = %e,
                    "failed to apply event; halting so the durable cursor cannot advance past it \
                     (after handling the deposit by hand, list it in EVENT_SKIP_EVENTS as \
                     chain_id:epoch:seq to move past it)"
                );
                self.halt(envelope.chain_id, envelope.cursor.seq);
                false
            }
        }
    }

    /// Stop the consumer after an unrecoverable apply failure: the hook if
    /// one is installed (tests), otherwise the process, so a supervisor
    /// restarts it and the durable cursor redelivers.
    fn halt(&self, chain_id: u64, seq: i64) {
        match &self.on_apply_failure {
            Some(hook) => hook(chain_id, seq),
            None => std::process::exit(1),
        }
    }

    /// Handle a single monitor event.
    #[allow(clippy::cognitive_complexity)]
    async fn handle_event(&self, event: MonitorEvent) -> Result<(), EventConsumerError> {
        match event {
            MonitorEvent::PaymentDetected(payment) => self.handle_payment_detected(payment).await,
            MonitorEvent::PaymentConfirmed(payment) => self.handle_payment_confirmed(payment).await,
            MonitorEvent::ReorgDetected(reorg) => self.handle_reorg_detected(reorg).await,
            // Log other events but don't process them
            MonitorEvent::MonitorStarted { chain_id } => {
                tracing::info!(chain_id, "Monitor started");
                Ok(())
            }
            MonitorEvent::MonitorStopped { chain_id } => {
                tracing::info!(chain_id, "Monitor stopped");
                Ok(())
            }
            MonitorEvent::MonitorError { chain_id, error } => {
                tracing::warn!(chain_id, error, "Monitor error");
                Ok(())
            }
            MonitorEvent::AddressWatched(info) => {
                tracing::debug!(
                    chain_id = info.chain_id,
                    address = %info.address,
                    invoice_id = %info.invoice_id,
                    "Address watched"
                );
                Ok(())
            }
            MonitorEvent::AddressUnwatched(info) => {
                tracing::debug!(
                    chain_id = info.chain_id,
                    address = %info.address,
                    "Address unwatched"
                );
                Ok(())
            }
            MonitorEvent::StatusReport(report) => {
                tracing::debug!(
                    chain_id = report.chain_id,
                    watched_count = report.watched_count,
                    current_block = report.current_block,
                    "Status report"
                );
                // Trigger invoice expiration check for this network
                self.trigger_expiration_check(report.chain_id).await;
                Ok(())
            }
        }
    }

    /// Trigger invoice expiration check.
    ///
    /// Called when block events are received. This is a non-blocking operation -
    /// errors are logged but don't stop event processing.
    ///
    /// With network-agnostic invoices, this checks all expired invoices regardless
    /// of which chain triggered the event.
    async fn trigger_expiration_check(&self, chain_id: u64) {
        let Some(cleanup_service) = &self.cleanup_service else {
            return;
        };

        match cleanup_service.check_chain(chain_id).await {
            Ok(count) => {
                if count > 0 {
                    tracing::debug!(
                        chain_id,
                        expired_count = count,
                        "Expired invoices on block event"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    chain_id,
                    error = %e,
                    "Failed to check expired invoices"
                );
            }
        }
    }

    /// Convert a payment amount (in smallest units) to invoice currency.
    ///
    /// Formula: (raw_amount / 10^decimals) / rate = invoice_currency_amount
    ///
    /// The rate represents: 1 invoice_currency = rate asset_units
    /// So to get invoice currency: asset_amount / rate
    ///
    /// A raw amount is up to 78 digits (`NUMERIC(78,0)`), well past what
    /// `rust_decimal::Decimal`'s 96-bit mantissa (~28-29 digits) can hold
    /// exactly. `BigDecimal` is arbitrary-precision, so parsing and the
    /// power-of-ten division never round; only the division by `rate` (a
    /// genuinely fractional value) can produce a non-terminating result, and
    /// that's inherent to rate conversion, not a precision bug.
    fn convert_payment_to_invoice_currency(
        &self,
        raw_amount: &str,
        rate_str: &str,
        decimals: u8,
    ) -> Result<String, String> {
        // Parse raw amount (in smallest units, e.g., wei)
        let raw: BigDecimal = raw_amount
            .parse()
            .map_err(|e| format!("Invalid raw amount '{}': {}", raw_amount, e))?;

        // Parse exchange rate
        let rate: BigDecimal = rate_str
            .parse()
            .map_err(|e| format!("Invalid rate '{}': {}", rate_str, e))?;

        if rate.is_zero() {
            return Err("Rate is zero, cannot convert".to_string());
        }

        // Convert to human-readable amount: raw / 10^decimals
        let divisor = Self::compute_decimal_divisor(decimals);
        let human_amount = raw / divisor;

        // Convert to invoice currency: human_amount / rate
        let invoice_amount = human_amount / rate;

        Ok(Self::format_amount(&invoice_amount))
    }

    /// Convert a smallest unit amount to human-readable format.
    ///
    /// Used for asset-denominated invoices where no rate conversion is needed.
    /// This is an exact power-of-ten division (moving the decimal point), so
    /// `BigDecimal` never rounds here regardless of how large `raw_amount` is.
    fn convert_smallest_to_human(&self, raw_amount: &str, decimals: u8) -> Result<String, String> {
        let raw: BigDecimal = raw_amount
            .parse()
            .map_err(|e| format!("Invalid raw amount '{}': {}", raw_amount, e))?;

        let divisor = Self::compute_decimal_divisor(decimals);
        let human_amount = raw / divisor;

        Ok(Self::format_amount(&human_amount))
    }

    /// Compute 10^decimals. `BigDecimal` is arbitrary-precision, so this is
    /// always exact and cannot overflow the way a fixed-mantissa type would.
    fn compute_decimal_divisor(decimals: u8) -> BigDecimal {
        BigDecimal::from(10u8).powi(i64::from(decimals))
    }

    /// Render an amount the way the rest of the system already reads it.
    ///
    /// Two things `BigDecimal::to_string` does that `rust_decimal` did not,
    /// and that reach a user:
    ///
    /// - It switches to scientific notation past five leading zeros, so one
    ///   wei of an 18-decimal token stringifies as `1E-18`. That value is
    ///   broadcast over the checkout and dashboard WebSocket, where a plain
    ///   decimal was shown before.
    /// - Division runs at 100 significant digits rather than 28, so a rate
    ///   conversion can produce a 100-plus character string. It is stored in
    ///   `NUMERIC(78,18)`, so everything past the 18th decimal is dropped on
    ///   write - rounding here means the number sent over the WebSocket and
    ///   the number the invoice API returns later are the same number.
    ///
    /// `normalized` strips the trailing zeros the fixed scale introduces, so
    /// a whole amount stays `1` rather than `1.000000000000000000`.
    fn format_amount(value: &BigDecimal) -> String {
        value
            .with_scale_round(18, RoundingMode::HalfUp)
            .normalized()
            .to_plain_string()
    }
}

/// Error type for event consumer operations.
#[derive(Debug, thiserror::Error)]
pub enum EventConsumerError {
    #[error("database error: {0}")]
    Database(#[from] types::RepositoryError),

    #[error("invalid data: {0}")]
    InvalidData(String),
}

/// Parse `chain_id:epoch:seq[,...]`. The epoch is part of the key because `seq` restarts under a new epoch, so a stale entry must not skip an unrelated event. A malformed entry is an error
/// rather than ignored: a typo that silently skipped nothing would leave the
/// operator believing the poison event was handled.
pub fn parse_skip_events(raw: &str) -> Result<Vec<(u64, i64, i64)>, String> {
    raw.split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut fields = part.split(':');
            let (Some(chain), Some(epoch), Some(seq), None) =
                (fields.next(), fields.next(), fields.next(), fields.next())
            else {
                return Err(format!("expected chain_id:epoch:seq, got {part:?}"));
            };
            let chain = chain
                .trim()
                .parse::<u64>()
                .map_err(|e| format!("bad chain_id in {part:?}: {e}"))?;
            let epoch = epoch
                .trim()
                .parse::<i64>()
                .map_err(|e| format!("bad epoch in {part:?}: {e}"))?;
            let seq = seq
                .trim()
                .parse::<i64>()
                .map_err(|e| format!("bad seq in {part:?}: {e}"))?;
            Ok((chain, epoch, seq))
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod skip_events_tests {
    use super::parse_skip_events;

    #[test]
    fn parses_triples_and_ignores_blanks() {
        assert_eq!(
            parse_skip_events(" 8453:7:12, 1:2:5 ,").unwrap(),
            vec![(8453, 7, 12), (1, 2, 5)]
        );
        assert!(parse_skip_events("").unwrap().is_empty());
    }

    #[test]
    fn rejects_malformed_entries() {
        assert!(parse_skip_events("8453:12").is_err());
        assert!(parse_skip_events("x:1:1").is_err());
        assert!(parse_skip_events("1:1:y").is_err());
    }
}
