//! Chain monitor - monitors a single EVM chain for payments.

mod config;
mod confirmations;
mod lifecycle;
mod processing;
mod rescan;
pub use processing::BACKFILL_MAX_BLOCKS;

pub use config::{ChainMonitorConfig, WatchedAddress};

use super::events::{MonitorEvent, PaymentDetected};
use super::source::BlockSource;
use crate::network::ChainConfig;
use alloy::primitives::{Address, B256};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{RwLock, broadcast, mpsc};
use tracing::debug;

/// State of a pending payment awaiting confirmations.
#[derive(Debug, Clone)]
struct PendingPayment {
    event: PaymentDetected,
    #[allow(dead_code)]
    last_check_block: u64,
}

/// Key for watched address map: (address, token_contract).
/// token_contract is None for native asset, Some(addr) for ERC20.
pub type WatchKey = (Address, Option<Address>);

/// Monitor for a single EVM chain.
pub struct ChainMonitor<S: BlockSource> {
    /// Chain configuration.
    chain_config: &'static ChainConfig,
    /// Monitor configuration.
    config: ChainMonitorConfig,
    /// Block source.
    source: Arc<S>,
    /// Addresses being watched, keyed by (address, token_contract).
    /// This allows the same address to be watched for different tokens.
    watched: RwLock<HashMap<WatchKey, WatchedAddress>>,
    /// Payments pending confirmation, keyed by `(tx_hash, tx_index)`.
    ///
    /// Keyed by transaction alone, two transfers batched into one transaction
    /// overwrote each other here, so only the last-inserted one was ever
    /// confirmed. The payments themselves stopped colliding when the database
    /// key gained `tx_index`; this is the same collision one layer up, and it
    /// left the other invoice fully funded and stuck in `Processing` forever,
    /// because `Processing -> Paid` happens only when a confirmation arrives.
    pending: RwLock<HashMap<(B256, i32), PendingPayment>>,
    /// When the block stream last delivered a block.
    ///
    /// Liveness of the subscription, which is a different question from how
    /// far behind the chain head the monitor is. A monitor catching up after a
    /// restart is far behind while receiving blocks perfectly well; a
    /// half-open WebSocket is exactly level and receiving nothing.
    last_block_at: RwLock<Instant>,
    /// When the `start` event loop last completed a `select!` iteration.
    ///
    /// Unlike `last_block_at`, this moves on *every* completed iteration -
    /// the confirmation-check tick as well as a delivered block - so it is
    /// the one signal that keeps advancing as long as the loop itself is
    /// alive. A watchdog running outside this loop (in the coordinator) polls
    /// it to notice the loop wedged on a single iteration, which nothing
    /// inside that same loop can ever detect.
    loop_alive_at: RwLock<Instant>,
    /// Last processed block.
    last_block: RwLock<Option<u64>>,
    /// Block hash at last processed block (for reorg detection).
    last_block_hash: RwLock<Option<B256>>,
    /// Event sender.
    event_tx: broadcast::Sender<MonitorEvent>,
    /// Shutdown signal.
    shutdown_tx: mpsc::Sender<()>,
    shutdown_rx: RwLock<Option<mpsc::Receiver<()>>>,
}

impl<S: BlockSource + 'static> ChainMonitor<S> {
    /// Create a new chain monitor.
    pub fn new(chain_config: &'static ChainConfig, source: S, config: ChainMonitorConfig) -> Self {
        let (event_tx, _) = broadcast::channel(1024);
        let (shutdown_tx, shutdown_rx) = mpsc::channel(1);

        Self {
            chain_config,
            config,
            source: Arc::new(source),
            watched: RwLock::new(HashMap::new()),
            pending: RwLock::new(HashMap::new()),
            last_block_at: RwLock::new(Instant::now()),
            loop_alive_at: RwLock::new(Instant::now()),
            last_block: RwLock::new(None),
            last_block_hash: RwLock::new(None),
            event_tx,
            shutdown_tx,
            shutdown_rx: RwLock::new(Some(shutdown_rx)),
        }
    }

    /// Get the chain ID.
    pub fn chain_id(&self) -> u64 {
        self.chain_config.chain_id
    }

    /// Get chain name.
    pub fn chain_name(&self) -> &str {
        self.chain_config.name
    }

    /// Subscribe to monitor events.
    pub fn subscribe(&self) -> broadcast::Receiver<MonitorEvent> {
        self.event_tx.subscribe()
    }

    /// Add an address to watch.
    pub async fn watch(&self, watched: WatchedAddress) {
        let key = (watched.address, watched.token_contract);
        let address = watched.address;
        let token = watched.token_contract;
        self.watched.write().await.insert(key, watched);
        debug!(
            chain_id = self.chain_id(),
            %address,
            token = ?token,
            "watching address"
        );
    }

    /// Remove an address from watch list.
    /// token_contract should be None for native, Some(addr) for ERC20.
    pub async fn unwatch(
        &self,
        address: &Address,
        token_contract: Option<Address>,
    ) -> Option<WatchedAddress> {
        let key = (*address, token_contract);
        let removed = self.watched.write().await.remove(&key);
        if removed.is_some() {
            debug!(
                chain_id = self.chain_id(),
                %address,
                token = ?token_contract,
                "unwatched address"
            );
        }
        removed
    }

    /// Get all watched addresses.
    pub async fn watched_addresses(&self) -> Vec<WatchedAddress> {
        self.watched.read().await.values().cloned().collect()
    }

    /// Get the current block number.
    pub async fn current_block(&self) -> crate::error::EvmResult<u64> {
        self.source.get_block_number().await
    }
}

impl<S: BlockSource> std::fmt::Debug for ChainMonitor<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChainMonitor")
            .field("chain", &self.chain_config.name)
            .field("chain_id", &self.chain_config.chain_id)
            .finish()
    }
}
