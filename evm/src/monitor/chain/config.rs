//! Configuration and watched-address types for the chain monitor.

use crate::network::ChainConfig;
use alloy::primitives::{Address, U256};

/// Configuration for the chain monitor.
#[derive(Debug, Clone)]
pub struct ChainMonitorConfig {
    /// Number of confirmations required.
    pub required_confirmations: u64,
    /// Maximum blocks to scan per iteration.
    pub max_blocks_per_scan: u64,
    /// How often to check pending payments (seconds).
    pub confirmation_check_interval_secs: u64,
    /// How long the block stream may deliver nothing before it is assumed dead
    /// and resubscribed.
    ///
    /// This is a liveness threshold, not a lag one. A monitor catching up
    /// after a restart is far behind the head while receiving blocks perfectly
    /// well, and resubscribing then churns the provider's subscription for no
    /// benefit; a half-open WebSocket is exactly level and receiving nothing.
    ///
    /// Explicit rather than derived from the check interval, which is a
    /// different concern: shortening the interval to notice confirmations
    /// sooner should not also make the monitor quicker to tear down a healthy
    /// subscription. Default is comfortably longer than a block on any chain
    /// this monitors.
    pub stall_timeout_secs: u64,
    /// How long the event loop itself may go without completing a single
    /// `select!` iteration before it is treated as hung and the process
    /// exits so it can be restarted.
    ///
    /// Distinct from `stall_timeout_secs`: that one is checked *from inside*
    /// the loop, on the confirmation-check tick, so it can only ever fire
    /// while the loop is still cycling. It cannot help when the loop itself
    /// is wedged - stuck awaiting an RPC call inside `process_block` or
    /// `check_confirmations` that never returns - because the tick that
    /// would notice never comes either. There is no in-process fix for a
    /// hung await; exiting is the only thing guaranteed to work regardless
    /// of what it is stuck on. Default is comfortably longer than several
    /// confirmation-check intervals, so an ordinary slow tick never trips it.
    pub loop_hang_timeout_secs: u64,
    /// Whether to detect native (ETH) transfers.
    pub monitor_native: bool,
    /// Whether to detect ERC20 transfers.
    pub monitor_erc20: bool,
    /// Largest `[from_block, tip]` window a `RescanFrom` command may cover.
    ///
    /// Unlike [`crate::monitor::BACKFILL_MAX_BLOCKS`], which chunks a scan
    /// the monitor has already accepted, this is a refusal threshold: a
    /// cursor from a long-dead deployment can name a height millions of
    /// blocks back, and scanning that is unbounded cost (one RPC call per
    /// block for native transfers alone). A window wider than this is
    /// refused outright rather than partially scanned, so a caller cannot
    /// mistake a truncated scan for a complete one.
    pub rescan_max_blocks: u64,
}

impl Default for ChainMonitorConfig {
    fn default() -> Self {
        Self {
            required_confirmations: 12,
            max_blocks_per_scan: 100,
            confirmation_check_interval_secs: 30,
            stall_timeout_secs: 120,
            loop_hang_timeout_secs: 300,
            monitor_native: true,
            monitor_erc20: true,
            rescan_max_blocks: 10_000,
        }
    }
}

impl ChainMonitorConfig {
    /// Create config from chain defaults.
    pub fn from_chain(chain: &ChainConfig) -> Self {
        Self {
            required_confirmations: chain.confirmations_required as u64,
            ..Default::default()
        }
    }
}

/// An address being watched for payments.
#[derive(Debug, Clone)]
pub struct WatchedAddress {
    /// The address to watch.
    pub address: Address,
    /// Invoice ID this address is for.
    pub invoice_id: uuid::Uuid,
    /// Expected amount (if known).
    pub expected_amount: Option<U256>,
    /// Token contract (None = native).
    pub token_contract: Option<Address>,
    /// When watching started.
    pub created_at: chrono::DateTime<chrono::Utc>,
}
