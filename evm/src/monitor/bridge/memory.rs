//! In-memory event bridge using a durable outbox behind a mutex.
//!
//! Useful for testing and single-process deployments where the monitor
//! runs in the same process as the API server.
//!
//! Supports bidirectional communication:
//! - Events flow from monitor to API server, through a durable outbox that
//!   survives a consumer dropping its stream and resubscribing (unlike the
//!   commands side, which stays a plain broadcast: `watch_retry` already
//!   covers a lost command).
//! - Commands flow from API server to monitor

use super::{CommandStream, DurableEventStream, EventBridge, EventCursor, EventEnvelope};
use crate::error::{EvmError, EvmResult};
use crate::monitor::events::{MonitorCommand, MonitorEvent};
use async_stream::stream;
use async_trait::async_trait;
use rand::Rng;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tokio::sync::{Notify, broadcast};
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;
use tracing::error;

/// Mint a fresh epoch identity.
///
/// Mirrors `RedisBridge::random_epoch` (duplicated rather than shared: that
/// one lives behind the `redis` feature gate, this bridge does not). A fresh
/// `MemoryBridge` is exactly the "single-process deployment restarted" case
/// the epoch mechanism exists to catch - a fixed starting value would
/// silently match whatever epoch a prior process instance had persisted to
/// `chain_cursors`, so a resume would proceed against a brand-new, empty
/// outbox as if it were a continuation of the old one.
fn random_epoch() -> i64 {
    rand::rng().random_range(1..=i64::MAX)
}

/// The event outbox: every retained published envelope, in publish order.
///
/// A queue behind a `Mutex` rather than a broadcast channel because the
/// whole point is that a consumer which was not subscribed when an event
/// was published can still see it later - a broadcast channel drops exactly
/// that message, which is the bug this bridge exists to not reproduce.
///
/// `next_seq` is the seq the *next* published entry will get; it keeps
/// counting up even past what `max_retained` lets `entries` hold, the same
/// way a real outbox's sequence counter is a separate key from its retained
/// data. The oldest retained seq is always `next_seq - entries.len()`.
struct Outbox {
    epoch: i64,
    entries: VecDeque<EventEnvelope>,
    next_seq: i64,
    /// `None` means unbounded (production default). `Some(n)` caps
    /// retention at `n` entries, trimming the oldest first - see
    /// [`MemoryBridge::with_max_retained`].
    max_retained: Option<usize>,
}

/// In-memory event bridge with a durable event outbox.
pub struct MemoryBridge {
    outbox: Arc<Mutex<Outbox>>,
    /// Woken on every publish so a live `subscribe_from` tail notices new
    /// entries without polling.
    notify: Arc<Notify>,
    /// Commands channel (API server -> monitor). Fire-and-forget is fine
    /// here: `watch_retry` re-drives a lost command within 30 seconds.
    commands_tx: broadcast::Sender<MonitorCommand>,
}

impl MemoryBridge {
    /// Create a new in-memory bridge.
    pub fn new() -> Self {
        Self::with_capacity(4096)
    }

    /// Create a new in-memory bridge with specified capacity for the
    /// commands channel. The event outbox is unbounded: durability is the
    /// point, so there is nothing safe to drop from it here.
    pub fn with_capacity(capacity: usize) -> Self {
        Self::new_inner(capacity, None)
    }

    /// Create a new in-memory bridge that only retains the last
    /// `max_retained` entries.
    ///
    /// Test-only in practice, mirroring `RedisBridge::new_with_maxlen`: it
    /// lets a test force `subscribe_from` to report `OUT_OF_RANGE` by
    /// publishing a handful of entries past a small cap, rather than
    /// needing a real Redis to reproduce retention loss.
    pub fn with_max_retained(max_retained: usize) -> Self {
        Self::new_inner(4096, Some(max_retained))
    }

    fn new_inner(commands_capacity: usize, max_retained: Option<usize>) -> Self {
        let (commands_tx, _) = broadcast::channel(commands_capacity);
        Self {
            outbox: Arc::new(Mutex::new(Outbox {
                epoch: random_epoch(),
                entries: VecDeque::new(),
                next_seq: 0,
                max_retained,
            })),
            notify: Arc::new(Notify::new()),
            commands_tx,
        }
    }

    /// Get a raw broadcast sender for commands (for direct use).
    pub fn commands_sender(&self) -> broadcast::Sender<MonitorCommand> {
        self.commands_tx.clone()
    }
}

impl Default for MemoryBridge {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl EventBridge for MemoryBridge {
    // =========================================================================
    // Events (Monitor -> API Server)
    // =========================================================================

    async fn publish(&self, event: &MonitorEvent) -> EvmResult<()> {
        {
            let mut outbox = self.outbox.lock().expect("outbox mutex poisoned");
            let seq = outbox.next_seq;
            outbox.next_seq += 1;
            let cursor = EventCursor {
                epoch: outbox.epoch,
                seq,
                block_height: event.block_height() as i64,
            };
            outbox.entries.push_back(EventEnvelope {
                chain_id: event.chain_id(),
                cursor,
                event: event.clone(),
            });
            if let Some(max) = outbox.max_retained {
                while outbox.entries.len() > max {
                    outbox.entries.pop_front();
                }
            }
        }
        self.notify.notify_waiters();
        Ok(())
    }

    async fn subscribe_from(&self, from: Option<EventCursor>) -> EvmResult<DurableEventStream> {
        // Mirrors `RedisBridge::subscribe_from`: same epoch does not, on its
        // own, mean `cursor.seq` is still retained, since trimming runs
        // independently of the epoch key. A consumer resuming from a seq
        // this outbox no longer holds must not silently start from whatever
        // happens to be retained next - it needs the loud `OUT_OF_RANGE`
        // path instead.
        if let Some(cursor) = from {
            let (oldest, next_seq) = {
                let outbox = self.outbox.lock().expect("outbox mutex poisoned");
                (
                    outbox.next_seq - outbox.entries.len() as i64,
                    outbox.next_seq,
                )
            };
            // Two ways the cursor can name a seq this outbox does not hold:
            // trimmed away behind the oldest retained entry, or beyond
            // anything ever published (a cursor from a different process
            // lifetime). Neither depends on `max_retained` being set, and
            // the second must be refused here: it would otherwise index
            // past the end of the retained entries.
            if cursor.seq < oldest - 1 || cursor.seq >= next_seq {
                // The epoch is deliberately left alone: bumping it would make
                // the consumer's next start read an epoch mismatch and
                // resume past this gap without an error. Left unchanged,
                // every restart fails the same way until an operator has
                // audited the gap.
                return Err(EvmError::EventStreamOutOfRange(format!(
                    "resume at seq {} is outside the retained range (seq {oldest}..{next_seq})",
                    cursor.seq
                )));
            }
        }

        let outbox = Arc::clone(&self.outbox);
        let notify = Arc::clone(&self.notify);
        let mut next_seq = from.map(|c| c.seq + 1).unwrap_or(0);
        // Separate from `next_seq`: `None` means "never yielded an entry
        // yet," which is the one case a low `next_seq` against a
        // higher-than-zero `oldest_retained` is *not* a gap - a fresh
        // `from: None` subscriber is supposed to start wherever the outbox
        // currently retains, same as `next_seq`'s `.unwrap_or(0)` above.
        // Once the first entry is yielded this tracks the real expectation,
        // the same way `RedisBridge::subscribe_from`'s `expected_seq` does.
        let mut expected_seq = from.map(|c| c.seq + 1);

        let s = stream! {
            loop {
                // Register interest before checking, so a publish landing
                // between the check and the await is never missed.
                let notified = notify.notified();

                let batch: Vec<EventEnvelope> = {
                    let guard = outbox.lock().expect("outbox mutex poisoned");
                    let oldest_retained = guard.next_seq - guard.entries.len() as i64;
                    // A reader that falls behind while already subscribed
                    // (stalled long enough that `max_retained` trims entries
                    // it hasn't read yet) must not have that gap clamped
                    // away by `.max(0)` below: that would silently resume
                    // from whatever is still retained, skipping the trimmed
                    // entries with no error - the "starting from wherever"
                    // failure this outbox exists to rule out, just reached
                    // mid-stream instead of at resume time.
                    if let Some(expected) = expected_seq
                        && expected < oldest_retained
                    {
                        error!(
                            expected_seq = expected,
                            oldest_retained,
                            "event stream gap detected mid-subscription; ending the stream \
                             rather than silently skipping the trimmed entries"
                        );
                        return;
                    }
                    let start = (next_seq - oldest_retained).max(0) as usize;
                    guard.entries.range(start..).cloned().collect()
                };

                if batch.is_empty() {
                    notified.await;
                    continue;
                }

                for envelope in batch {
                    next_seq = envelope.cursor.seq + 1;
                    expected_seq = Some(next_seq);
                    yield envelope;
                }
            }
        };
        Ok(Box::pin(s))
    }

    async fn current_epoch(&self) -> EvmResult<i64> {
        Ok(self.outbox.lock().expect("outbox mutex poisoned").epoch)
    }

    async fn bump_epoch(&self) -> EvmResult<i64> {
        // The in-memory outbox is unbounded (see `Outbox`'s docs), so it
        // never has a real retention-loss case to report on its own. This
        // exists so a test can still exercise the epoch-mismatch resume path
        // through the real `EventBridge` API rather than fabricating a
        // mismatched epoch by hand.
        let mut outbox = self.outbox.lock().expect("outbox mutex poisoned");
        outbox.epoch += 1;
        Ok(outbox.epoch)
    }

    // =========================================================================
    // Commands (API Server -> Monitor)
    // =========================================================================

    async fn publish_command(&self, command: &MonitorCommand) -> EvmResult<()> {
        // Ignore send errors (no receivers is fine)
        let _ = self.commands_tx.send(command.clone());
        Ok(())
    }

    async fn subscribe_commands(&self) -> EvmResult<CommandStream> {
        let rx = self.commands_tx.subscribe();
        let stream = BroadcastStream::new(rx).filter_map(|result| result.ok());
        Ok(Box::pin(stream))
    }

    // =========================================================================
    // Utility
    // =========================================================================

    fn name(&self) -> &str {
        "MemoryBridge"
    }

    async fn health_check(&self) -> EvmResult<()> {
        // Always healthy
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor::events::{PaymentDetected, WatchAddressCommand};
    use alloy::primitives::{Address, B256, U256};
    use chrono::Utc;
    use tokio_stream::StreamExt;

    fn make_event() -> MonitorEvent {
        MonitorEvent::PaymentDetected(PaymentDetected {
            chain_id: 1,
            invoice_id: uuid::Uuid::new_v4(),
            payment_address: Address::ZERO,
            amount: U256::from(1000),
            tx_hash: B256::ZERO,
            block_number: 100,
            block_hash: B256::ZERO,
            log_index: None,
            is_native: true,
            token_address: None,
            from_address: Address::ZERO,
            confirmations: 1,
            required_confirmations: 12,
            detected_at: Utc::now(),
        })
    }

    fn make_command() -> MonitorCommand {
        MonitorCommand::WatchAddress(WatchAddressCommand {
            chain_id: 1,
            address: Address::ZERO,
            invoice_id: uuid::Uuid::new_v4(),
            expected_amount: Some(U256::from(1000)),
            token_contract: None,
        })
    }

    #[tokio::test]
    async fn test_memory_bridge_events_durable() {
        let bridge = MemoryBridge::new();

        // Subscribe first
        let mut stream = bridge.subscribe_from(None).await.unwrap();

        // Publish event
        let event = make_event();
        bridge.publish(&event).await.unwrap();

        // Receive event
        let received = tokio::time::timeout(std::time::Duration::from_millis(100), stream.next())
            .await
            .unwrap()
            .unwrap();

        match received.event {
            MonitorEvent::PaymentDetected(p) => {
                assert_eq!(p.chain_id, 1);
            }
            _ => panic!("unexpected event type"),
        }
        assert_eq!(received.cursor.seq, 0);
    }

    #[tokio::test]
    async fn test_memory_bridge_events_survive_a_late_subscriber() {
        let bridge = MemoryBridge::new();

        // Nobody is subscribed yet when this publishes - the point of a
        // durable outbox is that this is not lost, unlike plain pub/sub.
        bridge.publish(&make_event()).await.unwrap();

        let mut stream = bridge.subscribe_from(None).await.unwrap();
        let received = tokio::time::timeout(std::time::Duration::from_millis(100), stream.next())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(received.cursor.seq, 0);
    }

    #[tokio::test]
    async fn test_memory_bridge_resumes_after_a_cursor() {
        let bridge = MemoryBridge::new();

        bridge.publish(&make_event()).await.unwrap(); // seq 0
        bridge.publish(&make_event()).await.unwrap(); // seq 1
        bridge.publish(&make_event()).await.unwrap(); // seq 2

        let cursor = EventCursor {
            epoch: bridge.current_epoch().await.unwrap(),
            seq: 0,
            block_height: 0,
        };
        let mut stream = bridge.subscribe_from(Some(cursor)).await.unwrap();

        let first = tokio::time::timeout(std::time::Duration::from_millis(100), stream.next())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.cursor.seq, 1);

        let second = tokio::time::timeout(std::time::Duration::from_millis(100), stream.next())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(second.cursor.seq, 2);
    }

    #[tokio::test]
    async fn test_memory_bridge_commands_pubsub() {
        let bridge = MemoryBridge::new();

        // Subscribe to commands first
        let mut stream = bridge.subscribe_commands().await.unwrap();

        // Publish command
        let command = make_command();
        bridge.publish_command(&command).await.unwrap();

        // Receive command
        let received = tokio::time::timeout(std::time::Duration::from_millis(100), stream.next())
            .await
            .unwrap()
            .unwrap();

        match received {
            MonitorCommand::WatchAddress(w) => {
                assert_eq!(w.chain_id, 1);
            }
            _ => panic!("unexpected command type"),
        }
    }

    #[tokio::test]
    async fn a_reader_stalled_past_the_retention_window_sees_the_stream_end_not_a_gap() {
        let bridge = MemoryBridge::with_max_retained(3);

        bridge.publish(&make_event()).await.unwrap(); // seq 0
        bridge.publish(&make_event()).await.unwrap(); // seq 1
        bridge.publish(&make_event()).await.unwrap(); // seq 2

        let mut stream = bridge.subscribe_from(None).await.unwrap();
        // Drain everything retained so far - the point where the generator
        // goes back to sleep waiting for the next publish, mirroring a
        // consumer that is caught up and then stalls.
        for expected_seq in 0..3 {
            let envelope =
                tokio::time::timeout(std::time::Duration::from_millis(100), stream.next())
                    .await
                    .unwrap()
                    .unwrap();
            assert_eq!(envelope.cursor.seq, expected_seq);
        }

        // While the reader is stalled (not polling `.next()`), enough
        // publishes land to evict seq 0 through 4 out of the 3-entry
        // retention window - the same effect a real outbox's `MAXLEN` trim
        // has on a consumer that falls behind.
        for _ in 0..5 {
            bridge.publish(&make_event()).await.unwrap(); // seq 3..=7
        }

        // Before the fix this silently resumed at seq 5 (the oldest still
        // retained), skipping seq 3 and 4 with no error. It must end the
        // stream instead.
        let next = tokio::time::timeout(std::time::Duration::from_millis(100), stream.next())
            .await
            .unwrap();
        assert!(
            next.is_none(),
            "expected the stream to end on a mid-subscription gap, got {next:?}"
        );
    }

    #[tokio::test]
    async fn two_instances_do_not_share_an_epoch() {
        let a = MemoryBridge::new().current_epoch().await.unwrap();
        let b = MemoryBridge::new().current_epoch().await.unwrap();
        assert_ne!(a, b);
    }

    #[tokio::test]
    async fn resuming_from_a_cursor_never_held_is_refused_not_a_panic() {
        // Unbounded (`max_retained` is `None`) and empty: the cursor names a
        // seq this instance never published.
        let bridge = MemoryBridge::new();
        let cursor = EventCursor {
            epoch: bridge.current_epoch().await.unwrap(),
            seq: 41,
            block_height: 0,
        };
        let res = bridge.subscribe_from(Some(cursor)).await;
        assert!(matches!(res, Err(EvmError::EventStreamOutOfRange(_))));

        // The newest published seq is still a valid resume point.
        bridge.publish(&make_event()).await.unwrap(); // seq 0
        let ok = EventCursor {
            epoch: cursor.epoch,
            seq: 0,
            block_height: 0,
        };
        let mut stream = bridge.subscribe_from(Some(ok)).await.unwrap();
        bridge.publish(&make_event()).await.unwrap(); // seq 1
        let next = stream.next().await.unwrap();
        assert_eq!(next.cursor.seq, 1, "resume replays only what follows seq 0");

        // A cursor at the extreme is refused, not an arithmetic overflow.
        let max = EventCursor {
            epoch: cursor.epoch,
            seq: i64::MAX,
            block_height: 0,
        };
        let res = bridge.subscribe_from(Some(max)).await;
        assert!(matches!(res, Err(EvmError::EventStreamOutOfRange(_))));
    }

    #[tokio::test]
    async fn test_memory_bridge_multiple_subscribers() {
        let bridge = MemoryBridge::new();

        let mut stream1 = bridge.subscribe_from(None).await.unwrap();
        let mut stream2 = bridge.subscribe_from(None).await.unwrap();

        let event = make_event();
        bridge.publish(&event).await.unwrap();

        // Both should receive
        let r1 = stream1.next().await;
        let r2 = stream2.next().await;

        assert!(r1.is_some());
        assert!(r2.is_some());
    }
}
