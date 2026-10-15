//! Payment monitoring system for EVM chains.
//!
//! This module provides a flexible architecture for monitoring blockchain
//! transactions across multiple EVM chains and providers.
//!
//! # Architecture
//!
//! ```text
//! evmmonitor binary (one or more instances)
//!     └── MonitorCoordinator
//!             └── ChainMonitor (per chain)
//!                     └── BlockSource (trait - provider abstraction)
//!                             ├── RpcBlockSource (direct WS/HTTP)
//!                             ├── AlchemyBlockSource (Alchemy enhanced APIs)
//!                             └── InfuraBlockSource (Infura APIs)
//!     └── EventBridge (bidirectional communication)
//!             ├── Events: monitor -> API server (PaymentDetected, etc.)
//!             ├── Commands: API server -> monitor (WatchAddress, etc.)
//!             ├── RedisBridge (production - multiple instances)
//!             └── MemoryBridge (testing/single process)
//!
//! ethpayserver API (subscribes to events via bridge)
//! ```
//!
//! # Example - Running as separate binary
//!
//! ```ignore
//! // evmmonitor binary handles chain monitoring
//! // Configure via TOML or environment variables:
//! //   EVMMONITOR_CHAINS=1,137,42161
//! //   EVMMONITOR_REDIS_URL=redis://localhost:6379
//! //   EVMMONITOR_RPC_1=https://eth.llamarpc.com
//! //   EVMMONITOR_WS_1=wss://eth.llamarpc.com
//!
//! // API server subscribes to events:
//! use evm::monitor::bridge::{EventBridge, RedisBridge, COMMANDS_CHANNEL, EVENTS_CHANNEL};
//!
//! let bridge = RedisBridge::new("redis://localhost:6379", EVENTS_CHANNEL, COMMANDS_CHANNEL).await?;
//! let mut events = bridge.subscribe_from(None).await?;
//!
//! while let Some(envelope) = events.next().await {
//!     match envelope.event {
//!         MonitorEvent::PaymentDetected(p) => { /* update invoice */ }
//!         MonitorEvent::PaymentConfirmed(p) => { /* mark complete */ }
//!         _ => {}
//!     }
//!     // persist envelope.cursor as the applied position before reading the next one
//! }
//!
//! // API server sends commands to monitor:
//! use evm::monitor::events::{MonitorCommand, WatchAddressCommand};
//!
//! let cmd = MonitorCommand::WatchAddress(WatchAddressCommand {
//!     chain_id: 1,
//!     address: payment_address,
//!     invoice_id,
//!     expected_amount: Some(amount),
//!     token_contract: None,
//! });
//! bridge.publish_command(&cmd).await?;
//! ```

pub mod bridge;
mod chain;
mod coordinator;
pub mod events;
mod handlers;
mod source;
pub mod startup;

pub use source::rpc::{RpcBlockSource, RpcSourceConfig};
pub use source::{BlockNotification, BlockSource, ChainHealth, LogFilter, SourceStatus};
// pub use source::alchemy::AlchemyBlockSource;
// pub use source::infura::InfuraBlockSource;

#[cfg(feature = "redis")]
pub use bridge::RedisBridge;
pub use bridge::{
    COMMANDS_CHANNEL, CommandStream, DurableEventStream, EVENTS_CHANNEL, EventBridge, EventCursor,
    EventEnvelope, MemoryBridge,
};

pub use chain::{BACKFILL_MAX_BLOCKS, ChainMonitor, ChainMonitorConfig, WatchedAddress};
pub use coordinator::{CoordinatorConfig, MonitorCoordinator};
pub use events::{
    AddressUnwatched, AddressWatched, GetStatusCommand, MonitorCommand, MonitorEvent,
    PaymentConfirmed, PaymentDetected, ReorgDetected, StatusReport, UnwatchAddressCommand,
    WatchAddressCommand,
};
pub use handlers::{EventHandler, EventHandlerFn, LoggingHandler};

#[cfg(any(test, feature = "test-utils"))]
pub use source::mock::{
    self as mock_source, MockBlockSource, make_block, make_block_with_parent,
    make_erc20_transfer_log, make_native_transfer,
};
