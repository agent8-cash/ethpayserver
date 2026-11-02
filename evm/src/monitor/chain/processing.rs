//! Block processing: native and ERC20 payment detection.

use super::{ChainMonitor, PendingPayment, WatchKey, WatchedAddress};
use crate::error::EvmResult;
use crate::monitor::events::{MonitorEvent, PaymentDetected};
use crate::monitor::source::{BlockNotification, BlockSource, LogFilter};
use alloy::primitives::{Address, B256, U256};
use chrono::Utc;
use std::collections::HashMap;
use tracing::{debug, info, warn};

/// Most blocks scanned to close a gap in one `process_block` call. Kept below
/// the span providers accept for a single log query.
pub const BACKFILL_MAX_BLOCKS: u64 = 200;

impl<S: BlockSource + 'static> ChainMonitor<S> {
    /// Process a new block.
    #[allow(clippy::cognitive_complexity)] // reorg check + watched-address scan is one logical unit
    pub(super) async fn process_block(&self, block: &BlockNotification) -> EvmResult<()> {
        let chain_id = self.chain_id();
        debug!(chain_id, block = block.number, "processing block");

        // Check for reorg
        if let Some(last_hash) = *self.last_block_hash.read().await
            && let Some(last_num) = *self.last_block.read().await
        {
            // The common case: this block is the immediate successor of the
            // last one we processed, so its parent hash must match exactly.
            //
            // Any other arrival — a gap (blocks skipped, e.g. catching up
            // after a stall) or a block at or behind a height we already
            // processed — means `parent_hash` cannot be compared directly
            // against `last_hash`. That used to mean no check ran at all, so
            // a fork arriving more than one block ahead went unnoticed. Ask
            // the chain instead whether the block we last processed is still
            // canonical.
            let fork_block = if block.number == last_num + 1 {
                (block.parent_hash != last_hash).then_some(last_num)
            } else {
                // A failure here must not be treated as "no reorg": `Ok(_)
                // => None` and a swallowed `Err` are indistinguishable to
                // the caller, but only one of them actually checked. Silently
                // falling through to `None` would advance `last_block` below
                // as if continuity were confirmed, permanently losing the one
                // chance to catch a reorg that coincided with an RPC hiccup.
                // Propagating instead leaves `last_block`/`last_block_hash`
                // untouched, so the same gap is re-checked on the next block
                // — the same fail-closed, free-retry pattern `handle_reorg`
                // uses for re-validation failures.
                match self.source.get_block_hash(last_num).await {
                    // `min(last_num, block.number)` is a best-effort guess,
                    // not a verified fork point: this call only tells us
                    // `last_num`'s canonical hash changed, and for a block
                    // arriving *behind* `last_num` (rather than the gap-ahead
                    // case this branch mainly exists for) we have no recorded
                    // hash below `last_num` to check against. If the true
                    // fork is deeper than `block.number`, this under-guesses
                    // it and misses candidates between the true fork and
                    // `block.number` — the dangerous direction. Closing that
                    // would need retained per-block history to walk back
                    // through, which this monitor does not keep; accepted as
                    // residual scope for the rare backward-jump case (see
                    // `test_reorg_backward_jump_guesses_fork_block_from_incoming_block_number`).
                    Ok(Some(hash)) if hash != last_hash => Some(last_num.min(block.number)),
                    Ok(_) => None,
                    Err(e) => {
                        warn!(
                            chain_id,
                            block = block.number,
                            last_num,
                            error = %e,
                            "failed to verify chain continuity across a block gap; will retry on the next block"
                        );
                        return Err(e);
                    }
                }
            };

            if let Some(fork_block) = fork_block {
                warn!(
                    chain_id,
                    block = block.number,
                    fork_block,
                    expected_parent = %last_hash,
                    actual_parent = %block.parent_hash,
                    "potential reorg detected"
                );
                self.handle_reorg(fork_block, last_hash, block).await?;
            }
        }

        // Blocks between the last one processed and this one were never
        // delivered (a resubscribe after a stall starts from the new head).
        // The reorg check above only asks whether the old block is still
        // canonical; it says nothing about what those blocks contained, so
        // read them here. Nothing else ever will: the cursor moves past them
        // below.
        let missed = match *self.last_block.read().await {
            Some(last_num) if block.number > last_num + 1 => Some((last_num + 1, block.number - 1)),
            _ => None,
        };

        // Snapshot the watched set so a long backfill does not hold the read
        // lock and stall `watch()` for every new invoice meanwhile.
        let watched = self.watched.read().await.clone();
        if watched.is_empty() {
            *self.last_block.write().await = Some(block.number);
            *self.last_block_hash.write().await = Some(block.hash);
            return Ok(());
        }

        if let Some((from, to)) = missed
            && !self.backfill_chunk(&watched, from, to, block).await?
        {
            return Ok(());
        }

        let mut detected = Vec::new();

        // Check for native transfers
        if self.config.monitor_native {
            self.check_native_payments(&watched, block.number, block.hash, &mut detected)
                .await?;
        }

        // Check for ERC20 transfers
        if self.config.monitor_erc20 {
            self.check_erc20_payments(&watched, block.number, block.number, &mut detected)
                .await?;
        }

        for event in detected {
            self.record_detected(event).await;
        }

        *self.last_block.write().await = Some(block.number);
        *self.last_block_hash.write().await = Some(block.hash);

        Ok(())
    }

    /// Read one bounded chunk of the gap `from..=to` before `block`, emit what
    /// it found and move the cursor to the last block scanned. Returns whether
    /// the chunk reached `to`; if not, the rest of the gap (and `block` with
    /// it) is picked up on the next arrival.
    ///
    /// A long stall is worked off across the blocks that follow, so no single
    /// handler runs for minutes and a failure costs at most the chunk in
    /// flight, never the chunks already completed.
    async fn backfill_chunk(
        &self,
        watched: &HashMap<WatchKey, WatchedAddress>,
        from: u64,
        to: u64,
        block: &BlockNotification,
    ) -> EvmResult<bool> {
        let chain_id = self.chain_id();
        let chunk_to = to.min(from + BACKFILL_MAX_BLOCKS - 1);
        info!(
            chain_id,
            from,
            to = chunk_to,
            "scanning blocks missed while the block stream was down"
        );
        // Everything detected is collected first and emitted only once the
        // whole chunk has been read. A failure part-way returns before
        // `last_block` moves, so the chunk is read again on the next block;
        // emitting as each payment was found would report the ones already
        // seen twice.
        let mut detected = Vec::new();
        let chunk_hash = self
            .backfill_missed_blocks(watched, from, chunk_to, &mut detected)
            .await?;
        for event in detected {
            self.record_detected(event).await;
        }
        // The cursor names the last block actually scanned, and its hash is
        // that block's, so the next arrival's continuity check asks about the
        // right block.
        *self.last_block.write().await = Some(chunk_to);
        *self.last_block_hash.write().await = Some(chunk_hash);

        if chunk_to < to {
            warn!(
                chain_id,
                scanned_to = chunk_to,
                head = block.number,
                behind = block.number - chunk_to,
                "backfill is still catching up after a block stream stall"
            );
            return Ok(false);
        }
        Ok(true)
    }

    /// Scan `from..=to`, blocks the stream skipped, through both detection
    /// paths, appending what it finds to `detected`, and return the hash of
    /// block `to`. The span must not exceed [`BACKFILL_MAX_BLOCKS`]. A failure
    /// propagates so `last_block` stays put and the same range is retried on
    /// the next block; the caller emits nothing from a failed attempt.
    async fn backfill_missed_blocks(
        &self,
        watched: &HashMap<WatchKey, WatchedAddress>,
        from: u64,
        to: u64,
        detected: &mut Vec<PaymentDetected>,
    ) -> EvmResult<B256> {
        if self.config.monitor_native {
            for number in from..=to {
                // The hash is only used to label a detected payment; a node
                // that cannot name the block cannot be trusted to have
                // answered for it either, so retry rather than guess.
                let hash = self.block_hash_or_err(number).await?;
                self.check_native_payments(watched, number, hash, detected)
                    .await?;
            }
        }
        if self.config.monitor_erc20 {
            // The chunk is below providers' cap on the span of one log query.
            self.check_erc20_payments(watched, from, to, detected)
                .await?;
        }
        self.block_hash_or_err(to).await
    }

    pub(super) async fn block_hash_or_err(&self, number: u64) -> EvmResult<B256> {
        self.source.get_block_hash(number).await?.ok_or_else(|| {
            crate::error::EvmError::Rpc(format!(
                "block {number} not available while backfilling a gap"
            ))
        })
    }

    /// Check for native currency payments in one block.
    ///
    /// Reads the block once and matches its transfers against the watched
    /// set, which is one RPC call per block however many addresses are
    /// watched - the same shape `check_erc20_payments` gets from putting every
    /// address into a single log filter.
    ///
    /// It used to poll `eth_getBalance` for each watched address on every
    /// block to find which balances had grown, and only then read the block to
    /// find the transfers behind them. That was one call per address per
    /// block: linear in open invoices, on every chain, forever. `rpc_cost.rs`
    /// pins the shape.
    ///
    /// Dropping the balance poll costs no detection, which is not obvious and
    /// was verified rather than assumed. The poll could observe that a balance
    /// had risen, but attribution came from the block's transactions either
    /// way - so an increase with no matching transaction in the block emitted
    /// nothing, and then had its evidence erased when the polled balance was
    /// stored. A balance rise the block cannot explain - an internal transfer,
    /// where value moves from a contract rather than a top-level transaction -
    /// was therefore already lost silently, before and after this change.
    /// Crediting those needs attribution this path never had; it is tracked
    /// separately and is a fix, not a regression.
    pub(super) async fn check_native_payments(
        &self,
        watched: &HashMap<WatchKey, WatchedAddress>,
        block_number: u64,
        block_hash: B256,
        detected: &mut Vec<PaymentDetected>,
    ) -> EvmResult<()> {
        // Every watched native address, not only ones something changed for:
        // the block is read once regardless, so narrowing the set first would
        // buy nothing and cost the call that told us what to narrow it to.
        let invoice_map: HashMap<Address, uuid::Uuid> = watched
            .iter()
            .filter(|((_, token), _)| token.is_none())
            .map(|((address, _), watch)| (*address, watch.invoice_id))
            .collect();

        if invoice_map.is_empty() {
            return Ok(());
        }

        let addresses: Vec<Address> = invoice_map.keys().copied().collect();
        let transfers = self
            .source
            .find_native_transfers_to(block_number, &addresses)
            .await?;

        // Process each transfer found
        for transfer in transfers {
            let Some(&invoice_id) = invoice_map.get(&transfer.to) else {
                continue;
            };

            let event = PaymentDetected {
                chain_id: self.chain_id(),
                invoice_id,
                payment_address: transfer.to,
                amount: transfer.value,
                tx_hash: transfer.tx_hash,
                block_number,
                block_hash,
                log_index: None,
                is_native: true,
                token_address: None,
                from_address: transfer.from,
                confirmations: 1,
                required_confirmations: self.config.required_confirmations,
                detected_at: Utc::now(),
            };

            detected.push(event);
        }

        Ok(())
    }

    /// Check for ERC20 token payments in blocks `from..=to`.
    pub(super) async fn check_erc20_payments(
        &self,
        watched: &HashMap<WatchKey, WatchedAddress>,
        from: u64,
        to: u64,
        detected: &mut Vec<PaymentDetected>,
    ) -> EvmResult<()> {
        // Collect unique addresses we're watching (for ERC20, token must be Some)
        let watch_addresses: Vec<Address> = watched
            .keys()
            .filter(|(_, token)| token.is_some())
            .map(|(addr, _)| *addr)
            .collect();
        if watch_addresses.is_empty() {
            return Ok(());
        }

        // Query Transfer logs for the range
        let filter =
            LogFilter::erc20_transfers_to(watch_addresses.clone()).with_block_range(from, to);

        let logs = self.source.get_logs(&filter).await?;

        for log in logs {
            // Decode Transfer event
            // topic0 = Transfer signature (already filtered)
            // topic1 = from address
            // topic2 = to address
            // data = amount

            if log.topics().len() < 3 {
                continue;
            }

            let to_address = Address::from_slice(&log.topics()[2].as_slice()[12..]);
            let token_address = log.address();

            // Check if this is an (address, token) pair we're watching
            let key = (to_address, Some(token_address));
            if let Some(watch) = watched.get(&key) {
                let from_address = Address::from_slice(&log.topics()[1].as_slice()[12..]);
                let amount = U256::from_be_slice(log.data().data.as_ref());
                // A range query cannot attribute a log to `to` by guessing:
                // the wrong block means the wrong confirmation depth.
                let Some(log_block) = log.block_number else {
                    return Err(crate::error::EvmError::Rpc(format!(
                        "transfer log in blocks {from}..={to} has no block number"
                    )));
                };

                let event = PaymentDetected {
                    chain_id: self.chain_id(),
                    invoice_id: watch.invoice_id,
                    payment_address: to_address,
                    amount,
                    tx_hash: log.transaction_hash.unwrap_or(B256::ZERO),
                    block_number: log_block,
                    block_hash: log.block_hash.unwrap_or(B256::ZERO),
                    log_index: log.log_index.map(|i| i as u32),
                    is_native: false,
                    token_address: Some(log.address()),
                    from_address,
                    confirmations: 1,
                    required_confirmations: self.config.required_confirmations,
                    detected_at: Utc::now(),
                };

                detected.push(event);
            }
        }

        Ok(())
    }

    /// Log a detected payment, track it for confirmation and announce it.
    pub(super) async fn record_detected(&self, event: PaymentDetected) {
        if event.is_native {
            info!(
                chain_id = self.chain_id(),
                invoice_id = %event.invoice_id,
                address = %event.payment_address,
                amount = %event.amount,
                tx = %event.tx_hash,
                from = %event.from_address,
                "native payment detected"
            );
        } else {
            info!(
                chain_id = self.chain_id(),
                invoice_id = %event.invoice_id,
                to_address = %event.payment_address,
                token = ?event.token_address,
                amount = %event.amount,
                tx = %event.tx_hash,
                "ERC20 payment detected"
            );
        }

        // Add to pending for confirmation tracking, keyed by the transfer
        // rather than the transaction: one transaction can carry two
        // transfers to two different watched addresses, and keying by hash
        // alone meant the second detection evicted the first, so only one of
        // them was ever confirmed. `tx_index` is `None` only for an ERC20 log
        // the node returned without a log index, which is malformed rather
        // than native - tracking it on the native sentinel would evict a real
        // native transfer in the same transaction.
        match event.tx_index() {
            Some(tx_index) => {
                self.pending.write().await.insert(
                    (event.tx_hash, tx_index),
                    PendingPayment {
                        last_check_block: event.block_number,
                        event: event.clone(),
                    },
                );
            }
            None => {
                warn!(
                    chain_id = self.chain_id(),
                    tx = %event.tx_hash,
                    "ERC20 transfer has no log index; not tracking it for confirmation"
                );
            }
        }

        let _ = self.event_tx.send(MonitorEvent::PaymentDetected(event));
    }
}
