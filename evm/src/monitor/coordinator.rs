//! Monitor coordinator - manages multiple chain monitors.

use super::chain::{ChainMonitor, WatchedAddress};
use super::events::{MonitorEvent, RescanWatch};
use super::handlers::EventHandler;
use super::source::{BlockSource, ChainHealth};
use crate::error::{EvmError, EvmResult};
use alloy::primitives::Address;
use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{RwLock, broadcast};
use tokio::task::JoinHandle;
use tracing::{error, info};

/// What to do when the watchdog decides a chain monitor's event loop has
/// hung. Takes the chain ID and how long the loop had been stalled.
pub type HangHook = Arc<dyn Fn(u64, Duration) + Send + Sync>;

/// Configuration for the monitor coordinator.
#[derive(Clone, Default)]
pub struct CoordinatorConfig {
    /// Event channel capacity.
    pub event_channel_capacity: usize,
    /// Overrides the watchdog's reaction to a hung event loop. Production
    /// leaves this `None`, which logs and exits the process so Docker
    /// restarts it - there is no in-process fix for an arbitrary hung await.
    /// Tests set this to observe that the watchdog reached the decision
    /// without killing the test binary via `process::exit`.
    pub on_loop_hang: Option<HangHook>,
}

impl CoordinatorConfig {
    pub fn new() -> Self {
        Self {
            event_channel_capacity: 4096,
            on_loop_hang: None,
        }
    }
}

impl std::fmt::Debug for CoordinatorConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CoordinatorConfig")
            .field("event_channel_capacity", &self.event_channel_capacity)
            .field("on_loop_hang", &self.on_loop_hang.is_some())
            .finish()
    }
}

/// Coordinates multiple chain monitors.
///
/// The coordinator manages monitors for multiple chains, aggregates
/// their events, and routes them to registered handlers.
///
/// It is generic over the block source type `S`, allowing access to
/// the underlying chain monitors for command handling.
pub struct MonitorCoordinator<S: BlockSource + 'static> {
    config: CoordinatorConfig,
    /// Monitors by chain ID.
    monitors: RwLock<HashMap<u64, MonitorHandle<S>>>,
    /// Aggregated event sender.
    event_tx: broadcast::Sender<MonitorEvent>,
    /// Registered event handlers.
    handlers: RwLock<Vec<Arc<dyn EventHandler>>>,
    /// Handler task.
    handler_task: RwLock<Option<JoinHandle<()>>>,
    /// Phantom data for the block source type.
    _phantom: PhantomData<S>,
}

/// Handle to a running chain monitor.
struct MonitorHandle<S: BlockSource + 'static> {
    #[allow(dead_code)]
    chain_id: u64,
    chain_name: String,
    /// Reference to the actual monitor for command handling.
    monitor: Arc<ChainMonitor<S>>,
    task: JoinHandle<()>,
    /// Watches `monitor`'s event loop from outside it and exits the process
    /// if that loop stops making progress. Aborted alongside `task` so a
    /// deliberately removed chain doesn't outlive its monitor and trip the
    /// watchdog on a loop nothing is running anymore.
    watchdog_task: JoinHandle<()>,
    event_rx: broadcast::Receiver<MonitorEvent>,
}

impl<S: BlockSource + 'static> MonitorCoordinator<S> {
    /// Create a new coordinator.
    pub fn new(config: CoordinatorConfig) -> Self {
        let (event_tx, _) = broadcast::channel(config.event_channel_capacity);

        Self {
            config,
            monitors: RwLock::new(HashMap::new()),
            event_tx,
            handlers: RwLock::new(Vec::new()),
            handler_task: RwLock::new(None),
            _phantom: PhantomData,
        }
    }

    /// Subscribe to all monitor events.
    pub fn subscribe(&self) -> broadcast::Receiver<MonitorEvent> {
        self.event_tx.subscribe()
    }

    /// Register an event handler.
    pub async fn register_handler(&self, handler: Arc<dyn EventHandler>) {
        self.handlers.write().await.push(handler);
    }

    /// Add and start a chain monitor.
    pub async fn add_chain(&self, monitor: Arc<ChainMonitor<S>>) -> EvmResult<()> {
        let chain_id = monitor.chain_id();
        let chain_name = monitor.chain_name().to_string();

        // Check if already monitoring this chain
        if self.monitors.read().await.contains_key(&chain_id) {
            return Err(EvmError::Monitor(format!(
                "already monitoring chain {}",
                chain_id
            )));
        }

        // Subscribe to monitor events
        let event_rx = monitor.subscribe();

        // Start the monitor
        let monitor_clone = monitor.clone();
        let task = tokio::spawn(async move {
            if let Err(e) = monitor_clone.start().await {
                error!(chain_id, error = %e, "chain monitor failed");
            }
        });

        // Watch the monitor's event loop from outside it. The loop's own
        // confirmation-check tick can reconnect a dead subscription, but
        // that fix shares the loop's fate: if the loop itself wedges - stuck
        // awaiting an RPC call inside `process_block` or
        // `check_confirmations` that never returns - the tick that would
        // notice never comes either, and the container looks up and alive
        // while the loop is not. Nothing in-process can un-wedge an
        // arbitrary hung await, so this exits instead, and relies on
        // whatever restarts this process (Docker's restart policy) to bring
        // it back.
        let watchdog_monitor = monitor.clone();
        let on_hang = self.config.on_loop_hang.clone();
        let watchdog_task = tokio::spawn(async move {
            let hang_timeout = watchdog_monitor.loop_hang_timeout();
            let check_interval = (hang_timeout / 4).max(Duration::from_millis(50));
            let mut interval = tokio::time::interval(check_interval);
            loop {
                interval.tick().await;
                let stalled_for = watchdog_monitor.loop_stalled_for().await;
                if stalled_for >= hang_timeout {
                    error!(
                        chain_id,
                        stalled_secs = stalled_for.as_secs(),
                        "chain monitor event loop made no progress for too long; exiting so the process can be restarted"
                    );
                    match &on_hang {
                        Some(hook) => hook(chain_id, stalled_for),
                        None => std::process::exit(1),
                    }
                }
            }
        });

        // Store handle with reference to monitor
        self.monitors.write().await.insert(
            chain_id,
            MonitorHandle {
                chain_id,
                chain_name: chain_name.clone(),
                monitor,
                task,
                watchdog_task,
                event_rx,
            },
        );

        info!(chain_id, chain = chain_name, "chain monitor added");

        Ok(())
    }

    /// Get a chain monitor by chain ID.
    pub async fn get_chain(&self, chain_id: u64) -> Option<Arc<ChainMonitor<S>>> {
        self.monitors
            .read()
            .await
            .get(&chain_id)
            .map(|h| h.monitor.clone())
    }

    /// Get all monitored chain IDs.
    pub async fn chain_ids(&self) -> Vec<u64> {
        self.monitors.read().await.keys().copied().collect()
    }

    /// Get health information for all monitored chains.
    pub async fn get_all_health(&self) -> Vec<ChainHealth> {
        let monitors = self.monitors.read().await;
        let mut health = Vec::with_capacity(monitors.len());

        for handle in monitors.values() {
            health.push(handle.monitor.get_health().await);
        }

        // Sort by chain_id for consistent ordering
        health.sort_by_key(|h| h.chain_id);
        health
    }

    /// Remove and stop a chain monitor.
    pub async fn remove_chain(&self, chain_id: u64) -> EvmResult<()> {
        let handle = self
            .monitors
            .write()
            .await
            .remove(&chain_id)
            .ok_or_else(|| EvmError::Monitor(format!("chain {} not found", chain_id)))?;

        handle.task.abort();
        handle.watchdog_task.abort();
        info!(chain_id, chain = handle.chain_name, "chain monitor removed");

        Ok(())
    }

    /// Get list of monitored chain IDs.
    #[deprecated(since = "0.1.0", note = "use chain_ids() instead")]
    pub async fn monitored_chains(&self) -> Vec<u64> {
        self.chain_ids().await
    }

    /// Start the event routing loop.
    pub async fn start(self: std::sync::Arc<Self>) -> EvmResult<()> {
        let event_tx = self.event_tx.clone();
        let this = self.clone();

        // Collect receivers from all monitors
        let mut receivers: Vec<broadcast::Receiver<MonitorEvent>> = Vec::new();
        for handle in self.monitors.write().await.values_mut() {
            // We need to resubscribe since we can't move the receiver
            receivers.push(handle.event_rx.resubscribe());
        }

        let task = tokio::spawn(async move {
            loop {
                // Wait for events from any monitor
                for rx in &mut receivers {
                    match rx.try_recv() {
                        Ok(event) => {
                            // Forward to aggregated channel
                            let _ = event_tx.send(event.clone());

                            // Call handlers
                            let handlers = this.handlers.read().await;
                            for handler in handlers.iter() {
                                if let Err(e) = handler.handle(&event).await {
                                    error!(error = %e, "event handler error");
                                }
                            }
                        }
                        Err(broadcast::error::TryRecvError::Empty) => {
                            // No events
                        }
                        Err(broadcast::error::TryRecvError::Lagged(n)) => {
                            error!(lagged = n, "event receiver lagged");
                        }
                        Err(broadcast::error::TryRecvError::Closed) => {
                            // Channel closed
                        }
                    }
                }

                // Small sleep to prevent busy loop
                tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
            }
        });

        *self.handler_task.write().await = Some(task);

        info!("monitor coordinator started");

        Ok(())
    }

    /// Stop the coordinator and all monitors.
    pub async fn stop(&self) -> EvmResult<()> {
        // Stop handler task
        if let Some(task) = self.handler_task.write().await.take() {
            task.abort();
        }

        // Stop all monitors
        let chain_ids: Vec<u64> = self.monitors.read().await.keys().copied().collect();
        for chain_id in chain_ids {
            let _ = self.remove_chain(chain_id).await;
        }

        info!("monitor coordinator stopped");

        Ok(())
    }

    /// Watch an address on a specific chain.
    pub async fn watch(&self, chain_id: u64, watched: WatchedAddress) -> EvmResult<()> {
        if let Some(monitor) = self.get_chain(chain_id).await {
            monitor.watch(watched).await;
            Ok(())
        } else {
            Err(EvmError::Monitor(format!("chain {} not found", chain_id)))
        }
    }

    /// Unwatch an address.
    /// token_contract should be None for native, Some(addr) for ERC20.
    pub async fn unwatch(
        &self,
        chain_id: u64,
        address: &Address,
        token_contract: Option<Address>,
    ) -> EvmResult<Option<WatchedAddress>> {
        if let Some(monitor) = self.get_chain(chain_id).await {
            Ok(monitor.unwatch(address, token_contract).await)
        } else {
            Err(EvmError::Monitor(format!("chain {} not found", chain_id)))
        }
    }

    /// Re-scan `[from_block, tip]` on a chain for transfers to `watches`.
    /// See [`ChainMonitor::rescan_from`].
    pub async fn rescan_from(
        &self,
        chain_id: u64,
        from_block: u64,
        watches: Vec<RescanWatch>,
    ) -> EvmResult<()> {
        if let Some(monitor) = self.get_chain(chain_id).await {
            monitor.rescan_from(from_block, watches).await
        } else {
            Err(EvmError::Monitor(format!("chain {} not found", chain_id)))
        }
    }
}

impl<S: BlockSource + 'static> Default for MonitorCoordinator<S> {
    fn default() -> Self {
        Self::new(CoordinatorConfig::new())
    }
}

impl<S: BlockSource + 'static> std::fmt::Debug for MonitorCoordinator<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MonitorCoordinator")
            .field("config", &self.config)
            .finish()
    }
}
