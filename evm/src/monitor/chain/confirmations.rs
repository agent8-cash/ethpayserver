//! Confirmation tracking and reorg handling.

use super::ChainMonitor;
use crate::error::EvmResult;
use crate::monitor::events::{MonitorEvent, PaymentConfirmed, ReorgDetected};
use crate::monitor::source::{BlockNotification, BlockSource, LogFilter};
use alloy::primitives::{Address, B256};
use chrono::Utc;
use tracing::{info, warn};

impl<S: BlockSource + 'static> ChainMonitor<S> {
    /// Check confirmation status of pending payments.
    pub(super) async fn check_confirmations(&self) -> EvmResult<()> {
        let current_block = self.source.get_block_number().await?;
        let mut confirmed: Vec<(B256, i32)> = Vec::new();

        {
            let mut pending = self.pending.write().await;

            for ((tx_hash, tx_index), payment) in pending.iter_mut() {
                let confirmations = current_block.saturating_sub(payment.event.block_number) + 1;
                payment.event.confirmations = confirmations;

                if confirmations >= payment.event.required_confirmations {
                    confirmed.push((*tx_hash, *tx_index));

                    let confirm_event = PaymentConfirmed {
                        chain_id: payment.event.chain_id,
                        invoice_id: payment.event.invoice_id,
                        payment_address: payment.event.payment_address,
                        amount: payment.event.amount,
                        tx_hash: payment.event.tx_hash,
                        tx_index: *tx_index,
                        block_number: payment.event.block_number,
                        confirmations,
                        confirmed_at: Utc::now(),
                    };

                    info!(
                        chain_id = self.chain_id(),
                        invoice_id = %payment.event.invoice_id,
                        tx = %tx_hash,
                        confirmations,
                        "payment confirmed"
                    );

                    let _ = self
                        .event_tx
                        .send(MonitorEvent::PaymentConfirmed(confirm_event));
                }
            }

            // Remove confirmed payments
            for key in &confirmed {
                pending.remove(key);
            }
        }

        Ok(())
    }

    /// Handle a detected chain reorganization.
    ///
    /// `affected_invoices` is a best-effort hint drawn from payments this
    /// monitor still has in memory: it is empty right after a restart and
    /// never includes a payment that has already confirmed and dropped out
    /// of `pending`. The database, not this event, is the source of truth
    /// for which payments a reorg actually touches — see the server's
    /// reorg handler, which finds the real candidate set there.
    ///
    /// What this monitor *can* answer authoritatively is whether a specific
    /// transaction is still on chain: `survived_tx_hashes` re-scans
    /// `[fork_block, new_block.number]` for currently watched addresses, or
    /// every transaction in range when nothing is watched (see
    /// `find_survived_tx_hashes`), so a transaction the reorg merely
    /// relocated to a different block is reported as still present rather
    /// than assumed gone. Retracting one of those would be the opposite
    /// error — un-paying an invoice that is still genuinely paid.
    pub(super) async fn handle_reorg(
        &self,
        fork_block: u64,
        old_hash: B256,
        new_block: &BlockNotification,
    ) -> EvmResult<()> {
        // Find affected invoices (best-effort hint, see doc comment above)
        let pending = self.pending.read().await;
        let affected: Vec<uuid::Uuid> = pending
            .values()
            .filter(|p| p.event.block_number >= fork_block)
            .map(|p| p.event.invoice_id)
            .collect();
        drop(pending);

        let depth = new_block.number.saturating_sub(fork_block) + 1;

        // If we can't ask the chain what survived, we must not report the
        // reorg at all: the caller treats an unlisted candidate as "gone" and
        // retracts it, so an empty list here would silently retract every
        // payment at or above `fork_block` on a mere RPC hiccup — the exact
        // "opposite error" the ticket warns about, just triggered by a
        // transient failure instead of a naive implementation. Propagating
        // the error instead leaves `last_block`/`last_block_hash` untouched
        // (see `process_block`), so the same reorg is re-evaluated, and
        // re-validated, on the next block.
        let (survived_tx_hashes, survivors_verifiable) = self
            .find_survived_tx_hashes(fork_block, new_block.number)
            .await
            .inspect_err(|e| {
                warn!(
                    chain_id = self.chain_id(),
                    fork_block,
                    error = %e,
                    "failed to re-validate reorg against the chain; will retry on the next block"
                );
            })?;

        let event = ReorgDetected {
            chain_id: self.chain_id(),
            fork_block,
            old_hash,
            new_hash: new_block.hash,
            depth,
            affected_invoices: affected.clone(),
            survived_tx_hashes,
            survivors_verifiable,
            detected_at: Utc::now(),
        };

        warn!(
            chain_id = self.chain_id(),
            fork_block,
            depth,
            affected_count = affected.len(),
            "reorg detected"
        );

        let _ = self.event_tx.send(MonitorEvent::ReorgDetected(event));

        Ok(())
    }

    /// Re-scan `[from, to]` to find transactions a reorg relocated to a
    /// different block rather than dropped entirely.
    ///
    /// With a non-empty watch set this filters to currently-watched
    /// addresses, the cheap path (`eth_getLogs`/one `find_native_transfers_to`
    /// per block rather than fetching every transaction in range). Watched
    /// addresses are restored from persistence on monitor restart, so this
    /// still works after one — unlike `pending`, which starts empty. It
    /// misses a payment whose address has since been unwatched, which the
    /// empty-watch-set fallback below exists to close.
    ///
    /// With an empty watch set — the ordinary state of a quiet server once
    /// every invoice has settled and passed its grace period — there is
    /// nothing to filter *for*, but the chain's block contents don't depend
    /// on any address being watched: `scan_all_transactions` asks what is
    /// actually in `[from, to]` directly. A payment whose address was
    /// unwatched before a reorg arrived used to be unverifiable for exactly
    /// this reason — see `InvoiceCleanupService::paid_unwatch_grace_period_secs`,
    /// which only narrows that window, it does not close it — and is now
    /// checked the same way regardless of watch state.
    ///
    /// Returns the transactions still on chain in `[from, to]`, and whether
    /// the scan could verify anything at all. The second half matters as much
    /// as the first: the consumer retracts a payment it cannot find, so an
    /// empty list from a scan that checked nothing must not read as an empty
    /// list from a scan that checked everything. It is `false` today only on
    /// a genuine RPC failure (see `scan_all_transactions`), never merely
    /// because nothing was watched.
    async fn find_survived_tx_hashes(&self, from: u64, to: u64) -> EvmResult<(Vec<B256>, bool)> {
        let watched = self.watched.read().await;
        if watched.is_empty() {
            return self.scan_all_transactions(from, to).await;
        }
        let native_addresses: Vec<Address> = watched
            .keys()
            .filter(|(_, token)| token.is_none())
            .map(|(address, _)| *address)
            .collect();
        let erc20_addresses: Vec<Address> = watched
            .keys()
            .filter(|(_, token)| token.is_some())
            .map(|(address, _)| *address)
            .collect();
        drop(watched);

        // The window cannot be *truncated* to `max_blocks_per_scan`: a
        // relocated transaction can land anywhere in `[from, to]`, so dropping
        // part of the range silently loses survivors, and an unreported
        // survivor gets retracted by the caller — the opposite error, and the
        // worse one.
        //
        // It is chunked instead. One `eth_getLogs` spanning an arbitrarily
        // wide range is rejected outright by hosted providers ("more than
        // 10000 results"), and because this path fails closed that rejection
        // stalls the chain: `handle_reorg` errs, `process_block` errs,
        // `last_block` never advances, and every subsequent notification
        // retries the identical failing query. The monitor would stop
        // detecting payments entirely until someone intervened. Chunking keeps
        // the whole range covered while keeping each call inside what a
        // provider will answer.
        let mut survived = Vec::new();
        let chunk = self.config.max_blocks_per_scan.max(1);

        if !native_addresses.is_empty() {
            // Deliberately not capped, unlike the ERC20 branch's chunking.
            // Chunking splits one wide query into several that together cover
            // the same range; a cap would cover *less* of it. Since a survivor
            // this scan misses is retracted by the consumer, dropping blocks
            // trades an RPC-cost problem for a wrongly-retracted-payment
            // problem, which is the worse of the two.
            //
            // The cost is real - one `eth_getBalance`-style call per block,
            // run inline in `process_block` - and on a very wide window it can
            // hold the monitor's `select!` loop. The bound that belongs here is
            // on how wide a window is acted on at all, not on how much of an
            // accepted window gets checked.
            for block_number in from..=to {
                let transfers = self
                    .source
                    .find_native_transfers_to(block_number, &native_addresses)
                    .await?;
                survived.extend(transfers.into_iter().map(|t| t.tx_hash));
            }
        }

        if !erc20_addresses.is_empty() {
            let mut start = from;
            while start <= to {
                let end = start.saturating_add(chunk - 1).min(to);
                let filter = LogFilter::erc20_transfers_to(erc20_addresses.clone())
                    .with_block_range(start, end);
                let logs = self.source.get_logs(&filter).await?;
                survived.extend(logs.into_iter().filter_map(|l| l.transaction_hash));
                if end == to {
                    break;
                }
                start = end + 1;
            }
        }

        Ok((survived, true))
    }

    /// Re-scan `[from, to]` for every transaction actually included in each
    /// block, regardless of address.
    ///
    /// This is what `find_survived_tx_hashes` falls back to when nothing is
    /// watched: a transaction's presence in a block is a property of the
    /// transaction, not of who it paid, so it can be checked without an
    /// address to filter on at all. A payment the chain genuinely dropped
    /// then correctly comes back absent instead of the whole reorg being
    /// reported as unverifiable - the gap this closes is exactly a quiet
    /// server with no watched addresses, not a rare edge case.
    ///
    /// One `get_block` per height, same shape and cost as the native
    /// address-filtered path's one `find_native_transfers_to` per height - this
    /// does not add a new order of RPC cost, it reads the same block that
    /// path would have read anyway. A missing block is a hard error rather
    /// than "no transactions": conflating the two would retract a payment
    /// that is actually still there, in a block this scan merely failed to
    /// fetch - the opposite error, and the worse one.
    async fn scan_all_transactions(&self, from: u64, to: u64) -> EvmResult<(Vec<B256>, bool)> {
        let mut survived = Vec::new();
        for block_number in from..=to {
            let block = self.source.get_block(block_number).await?.ok_or_else(|| {
                crate::error::EvmError::Rpc(format!(
                    "block {block_number} not available while re-validating a reorg with \
                     nothing watched"
                ))
            })?;
            survived.extend(block.transactions.into_hashes_vec());
        }
        Ok((survived, true))
    }
}
