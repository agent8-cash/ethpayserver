//! Monitor lifecycle: health reporting, start/stop event loop.

use super::ChainMonitor;
use crate::error::{EvmError, EvmResult};
use crate::monitor::events::MonitorEvent;
use crate::monitor::source::{BlockSource, BlockStream, ChainHealth, SourceStatus};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tokio_stream::StreamExt;
use tracing::{error, info, warn};

/// Consecutive stall resubscribes with no block delivered before the stall is
/// treated as sustained and logged at ERROR, which pages. One resubscribe that
/// restores delivery is ordinary self-healing and stays a warning.
const SUSTAINED_STALL_RESUBSCRIBES: u32 = 2;

impl<S: BlockSource + 'static> ChainMonitor<S> {
    /// Get health information for this chain.
    pub async fn get_health(&self) -> ChainHealth {
        let status = self.source.status();
        let current_block = self.source.get_block_number().await.ok();
        let last_processed_block = *self.last_block.read().await;
        let watched_addresses = self.watched.read().await.len();

        // Consider healthy if connected and not lagging more than 10 blocks
        let is_healthy = status == SourceStatus::Connected
            && match (current_block, last_processed_block) {
                (Some(current), Some(last)) => current.saturating_sub(last) <= 10,
                (Some(_), None) => true, // Just started, not yet processed
                _ => false,
            };

        ChainHealth {
            chain_id: self.chain_id(),
            chain_name: self.chain_name().to_string(),
            status,
            current_block,
            last_processed_block,
            watched_addresses,
            is_healthy,
        }
    }

    /// Start the monitor.
    #[allow(clippy::cognitive_complexity)] // tokio::select! event loop with multiple branches
    pub async fn start(self: Arc<Self>) -> EvmResult<()> {
        let chain_id = self.chain_id();
        info!(
            chain_id,
            chain = self.chain_name(),
            "starting chain monitor"
        );

        // Get shutdown receiver
        let mut shutdown_rx = self
            .shutdown_rx
            .write()
            .await
            .take()
            .ok_or_else(|| EvmError::Monitor("monitor already started".to_string()))?;

        // Subscribe to blocks. `last_block_at` is constructed at `new()` time,
        // which can be well before this actually runs; reset it here so the
        // stall clock starts at the subscription that makes it meaningful,
        // not at construction - otherwise a slow gap between the two counts
        // against a connection that was never given a chance.
        let mut block_stream = self.source.subscribe_blocks().await?;
        *self.last_block_at.write().await = Instant::now();

        // Emit start event
        let _ = self
            .event_tx
            .send(MonitorEvent::MonitorStarted { chain_id });

        // Confirmation check timer
        let mut confirm_interval = tokio::time::interval(tokio::time::Duration::from_secs(
            self.config.confirmation_check_interval_secs,
        ));

        loop {
            // Written before the loop waits on anything, so it advances once
            // per completed iteration regardless of which branch fired. A
            // hang inside any branch below - `process_block`,
            // `check_confirmations`, `resubscribe_if_stalled` - freezes this
            // exactly where an in-loop watchdog cannot see it, because that
            // watchdog would need another iteration to run and none is
            // coming. The coordinator's watchdog task polls this from
            // outside the loop for that reason.
            *self.loop_alive_at.write().await = Instant::now();

            tokio::select! {
                // Shutdown signal
                _ = shutdown_rx.recv() => {
                    info!(chain_id, "chain monitor shutting down");
                    let _ = self.event_tx.send(MonitorEvent::MonitorStopped { chain_id });
                    break;
                }

                // New block
                Some(block_result) = block_stream.next() => {
                    match block_result {
                        Ok(block) => {
                            // Liveness of the *stream*, recorded before any
                            // processing: a block that arrives but fails to
                            // process still proves the subscription is alive,
                            // and resubscribing would not fix it.
                            *self.last_block_at.write().await = Instant::now();
                            self.resubscribes_without_block.store(0, Ordering::Relaxed);

                            if let Err(e) = self.process_block(&block).await {
                                error!(chain_id, error = %e, "error processing block");
                                let _ = self.event_tx.send(MonitorEvent::MonitorError {
                                    chain_id,
                                    error: e.to_string(),
                                });
                            }
                        }
                        Err(e) => {
                            error!(chain_id, error = %e, "block stream error");
                        }
                    }
                }

                // Confirmation check timer
                _ = confirm_interval.tick() => {
                    if let Err(e) = self.check_confirmations().await {
                        warn!(chain_id, error = %e, "error checking confirmations");
                    }

                    if let Err(e) = self.resubscribe_if_stalled(&mut block_stream).await {
                        warn!(chain_id, error = %e, "failed to resubscribe stalled block stream");
                    }
                }
            }
        }

        Ok(())
    }

    /// Stop the monitor.
    pub async fn stop(&self) -> EvmResult<()> {
        let _ = self.shutdown_tx.send(()).await;
        Ok(())
    }

    /// How long a silent subscription is tolerated before it is assumed dead.
    fn stall_timeout(&self) -> Duration {
        Duration::from_secs(self.config.stall_timeout_secs)
    }

    /// How long since the `start` event loop last completed an iteration.
    ///
    /// Meant to be polled from outside the loop - anything running on the
    /// loop's own timer shares its fate if the loop wedges.
    pub async fn loop_stalled_for(&self) -> Duration {
        self.loop_alive_at.read().await.elapsed()
    }

    /// How long the event loop may go without completing an iteration before
    /// it is treated as hung.
    pub fn loop_hang_timeout(&self) -> Duration {
        Duration::from_secs(self.config.loop_hang_timeout_secs)
    }

    /// Reconnect a block stream that has stopped delivering.
    ///
    /// Two failure shapes land here. A dropped or half-open WebSocket does not
    /// always deliver a close frame - sometimes the subscription just stops
    /// yielding blocks, with no error to log and nothing to `select!` on,
    /// leaving the RPC reachable (health checks that ask it directly still
    /// succeed) while the stream is dead. Or the connection itself goes down
    /// and nothing retries it: `subscribe_blocks` is otherwise called once, at
    /// startup.
    ///
    /// Both are liveness failures, which is why this asks about liveness
    /// rather than about `is_healthy`. That flag is false whenever the chain
    /// is lagging more than a few blocks, and lagging is what ordinary
    /// catch-up looks like - so keying on it would resubscribe on every tick
    /// while the monitor works through a backlog, churning the provider's
    /// subscription at precisely the moment it can least afford it, and doing
    /// nothing to help it catch up.
    ///
    /// Runs on the confirmation-check timer because that is the one thing
    /// already on a clock here.
    async fn resubscribe_if_stalled(&self, block_stream: &mut BlockStream) -> EvmResult<()> {
        let health = self.get_health().await;

        // `is_healthy` is the wrong question here. It is false whenever the
        // chain is lagging more than a few blocks, and lagging is what ordinary
        // catch-up looks like: after a restart, or a brief outage, the monitor
        // is behind and working through blocks that are arriving perfectly
        // well. Resubscribing then churns the provider's subscription on every
        // confirmation tick, at exactly the moment the monitor can least afford
        // it, and does nothing to help it catch up.
        //
        // What this exists to catch is a subscription that has stopped
        // delivering: a half-open WebSocket that yields no blocks and no error,
        // with nothing to select! on. That is a liveness question, not a
        // progress one, so ask it of the stream rather than of the lag.
        let disconnected = health.status != SourceStatus::Connected;
        let silent_for = self.last_block_at.read().await.elapsed();
        let stalled = silent_for >= self.stall_timeout();

        if !disconnected && !stalled {
            return Ok(());
        }

        let resubscribes = self
            .resubscribes_without_block
            .fetch_add(1, Ordering::Relaxed)
            + 1;
        if resubscribes >= SUSTAINED_STALL_RESUBSCRIBES {
            error!(
                chain_id = self.chain_id(),
                status = ?health.status,
                current_block = ?health.current_block,
                last_processed_block = ?health.last_processed_block,
                silent_for_secs = silent_for.as_secs(),
                resubscribes,
                reason = if disconnected { "disconnected" } else { "no blocks received" },
                "block stream still not delivering after repeated resubscribes"
            );
        } else {
            warn!(
                chain_id = self.chain_id(),
                status = ?health.status,
                current_block = ?health.current_block,
                last_processed_block = ?health.last_processed_block,
                silent_for_secs = silent_for.as_secs(),
                reason = if disconnected { "disconnected" } else { "no blocks received" },
                "block stream is not delivering; resubscribing"
            );
        }

        *block_stream = self.source.subscribe_blocks().await?;
        *self.last_block_at.write().await = Instant::now();
        Ok(())
    }
}
