//! Event bridge abstraction for inter-process communication.
//!
//! The bridge enables bidirectional communication between monitor processes
//! and API servers:
//!
//! - **Events** flow from monitors to API servers (PaymentDetected, etc.)
//! - **Commands** flow from API servers to monitors (WatchAddress, etc.)

/// Default Redis channel for events (monitor -> API server).
pub const EVENTS_CHANNEL: &str = "evmmonitor:events";
/// Default Redis channel for commands (API server -> monitor).
pub const COMMANDS_CHANNEL: &str = "evmmonitor:commands";

mod memory;
#[cfg(feature = "redis")]
mod redis;

#[cfg(feature = "redis")]
pub use self::redis::RedisBridge;
pub use memory::MemoryBridge;

use super::events::{MonitorCommand, MonitorEvent};
use crate::error::EvmResult;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::pin::Pin;
use tokio_stream::Stream;

/// Stream of monitor commands (API server -> monitor).
pub type CommandStream = Pin<Box<dyn Stream<Item = MonitorCommand> + Send>>;

/// Stream of durable event envelopes (monitor -> API server).
pub type DurableEventStream = Pin<Box<dyn Stream<Item = EventEnvelope> + Send>>;

/// A position in an adapter's durable event outbox.
///
/// `epoch` names which lineage `seq` belongs to. A lineage ends whenever the
/// adapter can no longer vouch for the continuity of its own outbox - first
/// ever start, or its durable store lost retention - because starting a new
/// lineage while keeping the old `seq` numbering would make a fresh history
/// indistinguishable from a replay of the old one. `block_height` is the
/// chain height as of this position; it is carried for diagnostics and any
/// future rescan, but nothing here reads it back to decide where to resume.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventCursor {
    pub epoch: i64,
    pub seq: i64,
    pub block_height: i64,
}

/// A [`MonitorEvent`] tagged with the outbox position it was published at.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventEnvelope {
    pub chain_id: u64,
    pub cursor: EventCursor,
    pub event: MonitorEvent,
}

/// Event bridge for bidirectional communication between monitors and API servers.
///
/// Implementations can use different backends (Redis, NATS, in-memory, etc.)
/// to facilitate communication between the monitor binary and API servers.
///
/// ## Channel Structure
///
/// - **Events channel** (`evmmonitor:events`): Monitor publishes, API server subscribes
/// - **Commands channel** (`evmmonitor:commands`): API server publishes, monitor subscribes
#[async_trait]
pub trait EventBridge: Send + Sync {
    // =========================================================================
    // Events (Monitor -> API Server)
    // =========================================================================

    /// Publish an event to the durable outbox.
    ///
    /// Called by the monitor when a payment is detected, confirmed, etc. The
    /// bridge assigns the event's outbox position; the caller does not
    /// control `seq` or `epoch`.
    async fn publish(&self, event: &MonitorEvent) -> EvmResult<()>;

    /// Resume the durable event outbox strictly after `from`.
    ///
    /// `None` means the caller has no prior cursor and accepts whatever the
    /// outbox currently retains from its oldest entry - correct for a
    /// consumer that has genuinely never run before, never for one that
    /// lost its cursor. A caller with a stored cursor whose `epoch` does not
    /// match [`EventBridge::current_epoch`] must not call this with that
    /// cursor: the outbox lineage it names may no longer exist.
    async fn subscribe_from(&self, from: Option<EventCursor>) -> EvmResult<DurableEventStream>;

    /// The outbox's current epoch.
    ///
    /// A caller reconciling a stored cursor calls this first: a mismatch
    /// against the cursor's `epoch` means that cursor's `seq` numbering may
    /// belong to a lineage this outbox no longer has.
    async fn current_epoch(&self) -> EvmResult<i64>;

    /// Invalidate the current outbox lineage and start a new one.
    ///
    /// Nothing in production calls this: a position the outbox no longer
    /// retains halts the consumer instead, and a real break arrives as the
    /// backing store losing its keyspace and minting a fresh epoch. It is
    /// on the trait so tests can simulate a lineage break against either
    /// bridge. The change is
    /// visible to every caller sharing this outbox, not just the one that
    /// detected the loss: the next [`EventBridge::current_epoch`] call from
    /// any of them sees the new value, so nobody resumes from the broken
    /// lineage without going through the loud path.
    async fn bump_epoch(&self) -> EvmResult<i64>;

    // =========================================================================
    // Commands (API Server -> Monitor)
    // =========================================================================

    /// Publish a command to the monitors.
    ///
    /// Called by API servers to instruct monitors (e.g., watch an address).
    async fn publish_command(&self, command: &MonitorCommand) -> EvmResult<()>;

    /// Subscribe to commands from the API server.
    ///
    /// Called by monitors to receive instructions (e.g., WatchAddress).
    async fn subscribe_commands(&self) -> EvmResult<CommandStream>;

    // =========================================================================
    // Utility
    // =========================================================================

    /// Get the bridge name for logging.
    fn name(&self) -> &str;

    /// Check if the bridge is connected/healthy.
    async fn health_check(&self) -> EvmResult<()>;
}
