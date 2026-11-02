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
    announce(
        capabilities.volume.publish(volume_reader.clone()),
        "plugins may read what an account settled over a window",
    );
    announce(
        capabilities.bulk_volume.publish(volume_reader),
        "plugins may read what many accounts settled in one call",
    );
    // Read-only: a plugin can read a standing and has no import to write one.
    announce(
        capabilities.standing.publish(state.data_service.clone()),
        "plugins may read an account's stored standing",
    );
    // Tells an account something through a channel and address the plugin never names.
    announce(
        capabilities
            .notifier
            .publish(Arc::new(PluginAccountNotifier::new(state.clone()))),
        "plugins may ask the host to notify an account",
    );
}

fn announce(published: bool, what: &str) {
    if published {
        tracing::info!("{what}");
    }
}
