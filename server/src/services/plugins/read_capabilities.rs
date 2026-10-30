//! Capabilities 6 and 7 (read-only): publishes the ones every instance can
//! answer, with or without an operator store.

use std::sync::Arc;

use auth::SessionService;

use super::{DeferredCapabilities, PluginAccountNotifier, PluginMerchantVolume};
use crate::state::PgAppState;

/// Publishes volume, standing and `account_notice`. None needs an operator
/// store: an instance with no store still has merchants with volume, and
/// every instance has its own user directory and email sender.
pub fn publish_unconditional_capabilities<A: SessionService + 'static>(
    state: &PgAppState<A>,
    capabilities: &DeferredCapabilities,
) {
    // One reader answers the single and batched forms, each under its own cell.
    let volume_reader = Arc::new(PluginMerchantVolume::new(state.clone()));
    if capabilities.volume.publish(volume_reader.clone()) {
        tracing::info!("plugins may read what an account settled over a window");
    }
    if capabilities.bulk_volume.publish(volume_reader) {
        tracing::info!("plugins may read what many accounts settled in one call");
    }

    // Read-only: a plugin can read a standing and has no import to write one.
    if capabilities.standing.publish(state.data_service.clone()) {
        tracing::info!("plugins may read an account's stored standing");
    }

    // Tells an account something through a channel and address the plugin never names.
    if capabilities
        .notifier
        .publish(Arc::new(PluginAccountNotifier::new(state.clone())))
    {
        tracing::info!("plugins may ask the host to notify an account");
    }
}
