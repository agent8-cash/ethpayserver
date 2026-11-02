//! `RescanFrom`: re-read a block window for an explicit watch set.
//!
//! Recovers a payment that confirmed while the event outbox had a lineage
//! break and whose watch has since expired: the monitor's in-memory watch
//! list no longer has it, so the caller passes the window's watch set
//! explicitly instead of this reading `watched`.

use super::{ChainMonitor, WatchKey, WatchedAddress};
use crate::error::{EvmError, EvmResult};
use crate::monitor::events::RescanWatch;
use crate::monitor::source::BlockSource;
use chrono::Utc;
use std::collections::HashMap;

impl<S: BlockSource + 'static> ChainMonitor<S> {
    /// Re-scan `[from_block, tip]` for native transfers and ERC-20
    /// `Transfer` logs to `watches`, publishing `PaymentDetected` (and, for
    /// anything that already has enough confirmations as of `tip`,
    /// `PaymentConfirmed` right after it) through the normal outbox.
    ///
    /// `watches` is matched exactly as given — not merged with, or filtered
    /// by, this monitor's in-memory watch list — so a caller can include
    /// addresses whose watch has since expired; the host, not this monitor,
    /// decides which addresses mattered during the window.
    ///
    /// Refuses outright, with no partial scan, when the window exceeds
    /// `config.rescan_max_blocks`: a cursor from a long-dead deployment can
    /// name a height millions of blocks back, and scanning that is
    /// unbounded cost (one RPC call per block for native transfers alone).
    pub async fn rescan_from(&self, from_block: u64, watches: Vec<RescanWatch>) -> EvmResult<()> {
        let chain_id = self.chain_id();
        let tip = self.source.get_block_number().await?;

        if from_block > tip {
            // Nothing to scan: the requested start is already past the
            // chain head.
            return Ok(());
        }

        let window = tip - from_block + 1;
        if window > self.config.rescan_max_blocks {
            return Err(EvmError::Monitor(format!(
                "rescan of chain {chain_id} blocks {from_block}..={tip} ({window} blocks) \
                 exceeds the cap of {} blocks; refusing rather than scanning part of the window",
                self.config.rescan_max_blocks
            )));
        }

        let watched: HashMap<WatchKey, WatchedAddress> = watches
            .into_iter()
            .map(|w| {
                let key = (w.address, w.token_contract);
                let value = WatchedAddress {
                    address: w.address,
                    invoice_id: w.invoice_id,
                    // Neither field matters for matching a rescanned
                    // transfer - detection keys only on (address,
                    // token_contract) and reads `invoice_id` - so there is
                    // no real value to carry over here.
                    expected_amount: None,
                    token_contract: w.token_contract,
                    created_at: Utc::now(),
                };
                (key, value)
            })
            .collect();

        if watched.is_empty() {
            return Ok(());
        }

        let mut detected = Vec::new();

        if self.config.monitor_native {
            for number in from_block..=tip {
                let hash = self.block_hash_or_err(number).await?;
                self.check_native_payments(&watched, number, hash, &mut detected)
                    .await?;
            }
        }

        if self.config.monitor_erc20 {
            // Chunked the same way `find_survived_tx_hashes` chunks its own
            // ERC20 re-scan: one `eth_getLogs` spanning the whole accepted
            // window is rejected outright by hosted providers once it gets
            // wide, and this window can be much wider than a single query's
            // cap even while staying inside `rescan_max_blocks`.
            let chunk = self.config.max_blocks_per_scan.max(1);
            let mut start = from_block;
            while start <= tip {
                let end = start.saturating_add(chunk - 1).min(tip);
                self.check_erc20_payments(&watched, start, end, &mut detected)
                    .await?;
                if end == tip {
                    break;
                }
                start = end + 1;
            }
        }

        // Every transfer this rescan found is announced as detected before
        // anything else happens to it: `handle_payment_confirmed` on the
        // consumer side looks a payment up by `(invoice_id, tx_hash,
        // tx_index)` and silently no-ops when no row exists yet, so a
        // confirmation emitted ahead of its own detection would be applied,
        // committed, and credit nothing. Recording every detection in one
        // pass, strictly before the single `check_confirmations` call below,
        // is what keeps every detection ahead of every confirmation it
        // enables for this rescan.
        for event in detected {
            self.record_detected(event).await;
        }

        // Anything just recorded that already has enough confirmations as
        // of `tip` is confirmed immediately, through the same path the live
        // monitor's timer uses, rather than waiting for the next tick to
        // notice a payment this rescan already read past its confirmation
        // depth.
        self.check_confirmations().await
    }
}
