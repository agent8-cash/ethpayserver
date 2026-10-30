//! Command handling and startup restoration of watched addresses.

use std::sync::Arc;

use chrono::Utc;
use data_service::{LiveWatchedAddressReader, LiveWatchedAddressWriter, RedisDataService};
use evm::monitor::bridge::EventBridge;
use evm::monitor::events::{AddressUnwatched, AddressWatched, StatusReport, WatchedAddressInfo};
use evm::monitor::{
    MonitorCommand, MonitorCoordinator, MonitorEvent, RpcBlockSource, WatchedAddress,
};
use tokio_stream::StreamExt;
use tracing::{debug, error, info, warn};
use types::InvoiceId;

/// Restore watched addresses from Redis on startup.
pub(crate) async fn restore_watched_addresses(
    coordinator: &Arc<MonitorCoordinator<RpcBlockSource>>,
    persistence: &Arc<RedisDataService>,
    monitored_chain_ids: &[u64],
) {
    match persistence.get_all_watched().await {
        Ok(addresses) => {
            let mut restored_count = 0;
            for (address, invoice_id, chain_id, token_address) in addresses {
                // This monitor is EVM-only and indexes chains by EIP-155
                // number; anything else cannot be one of ours.
                let Some(eip155) = chain_id.evm_chain_id() else {
                    warn!(chain_id = %chain_id, address = %address, "not an EVM chain; skipping");
                    continue;
                };

                // Only restore addresses for chains we're monitoring
                if !monitored_chain_ids.contains(&eip155) {
                    debug!(chain_id = %chain_id, address = %address, "skipping address for unmonitored chain");
                    continue;
                }

                if let Some(monitor) = coordinator.get_chain(eip155).await {
                    // Parse address - it's stored as lowercase hex string
                    let parsed_address = match address.parse() {
                        Ok(addr) => addr,
                        Err(e) => {
                            warn!(address = %address, error = %e, "failed to parse address");
                            continue;
                        }
                    };

                    // Parse invoice_id as UUID
                    let uuid = match uuid::Uuid::parse_str(invoice_id.as_str()) {
                        Ok(u) => u,
                        Err(e) => {
                            warn!(invoice_id = %invoice_id, error = %e, "failed to parse invoice_id as UUID");
                            continue;
                        }
                    };

                    // Parse token contract address if present
                    let token_contract: Option<alloy::primitives::Address> =
                        token_address.as_ref().and_then(|t| t.parse().ok());

                    let watched = WatchedAddress {
                        address: parsed_address,
                        invoice_id: uuid,
                        expected_amount: None,
                        token_contract,
                        created_at: Utc::now(),
                    };
                    monitor.watch(watched).await;
                    restored_count += 1;
                }
            }
            info!(
                count = restored_count,
                "restored watched addresses from persistence"
            );
        }
        Err(e) => {
            warn!(error = %e, "failed to restore watched addresses from persistence");
        }
    }
}

/// Handle incoming commands from the API server.
pub(crate) async fn handle_commands(
    mut commands: evm::monitor::bridge::CommandStream,
    coordinator: Arc<MonitorCoordinator<RpcBlockSource>>,
    bridge: Arc<dyn EventBridge>,
    persistence: Arc<RedisDataService>,
) {
    while let Some(command) = commands.next().await {
        debug!(?command, "received command");

        match command {
            MonitorCommand::WatchAddress(cmd) => {
                info!(
                    chain_id = cmd.chain_id,
                    address = %cmd.address,
                    invoice_id = %cmd.invoice_id,
                    "watch address command"
                );

                // Persist to Redis first (using chain_id directly for testnet support)
                let invoice_id = InvoiceId::from_string(cmd.invoice_id.to_string());
                let address_str = cmd.address.to_string().to_lowercase();
                let token_address_str = cmd.token_contract.map(|a| a.to_string().to_lowercase());
                if let Err(e) = persistence
                    .watch_address(
                        &address_str,
                        &invoice_id,
                        &types::ChainId::evm(cmd.chain_id),
                        token_address_str.as_deref(),
                    )
                    .await
                {
                    error!(error = %e, "failed to persist watched address");
                }

                // Find the chain monitor
                if let Some(monitor) = coordinator.get_chain(cmd.chain_id).await {
                    let watched = WatchedAddress {
                        address: cmd.address,
                        invoice_id: cmd.invoice_id.clone(),
                        expected_amount: cmd.expected_amount,
                        token_contract: cmd.token_contract,
                        created_at: Utc::now(),
                    };

                    monitor.watch(watched).await;

                    // Publish confirmation event
                    let event = MonitorEvent::AddressWatched(AddressWatched {
                        chain_id: cmd.chain_id,
                        address: cmd.address,
                        invoice_id: cmd.invoice_id,
                        watched_at: Utc::now(),
                    });

                    if let Err(e) = bridge.publish(&event).await {
                        error!(error = %e, "failed to publish AddressWatched event");
                    }
                } else {
                    warn!(
                        chain_id = cmd.chain_id,
                        "chain not monitored by this instance"
                    );
                }
            }

            MonitorCommand::UnwatchAddress(cmd) => {
                info!(
                    chain_id = cmd.chain_id,
                    address = %cmd.address,
                    "unwatch address command"
                );

                // Remove from persistence (using chain_id directly for testnet support)
                let address_str = cmd.address.to_string().to_lowercase();
                let token_address_str = cmd.token_contract.map(|a| a.to_string().to_lowercase());
                if let Err(e) = persistence
                    .unwatch_address(
                        &address_str,
                        &types::ChainId::evm(cmd.chain_id),
                        token_address_str.as_deref(),
                    )
                    .await
                {
                    debug!(error = %e, "failed to remove watched address from persistence");
                }

                if let Some(monitor) = coordinator.get_chain(cmd.chain_id).await {
                    let removed = monitor.unwatch(&cmd.address, cmd.token_contract).await;

                    // Publish confirmation event
                    let event = MonitorEvent::AddressUnwatched(AddressUnwatched {
                        chain_id: cmd.chain_id,
                        address: cmd.address,
                        invoice_id: removed.map(|w| w.invoice_id),
                    });

                    if let Err(e) = bridge.publish(&event).await {
                        error!(error = %e, "failed to publish AddressUnwatched event");
                    }
                } else {
                    warn!(
                        chain_id = cmd.chain_id,
                        "chain not monitored by this instance"
                    );
                }
            }

            MonitorCommand::RescanFrom(cmd) => {
                info!(
                    chain_id = cmd.chain_id,
                    from_block = cmd.from_block,
                    watches = cmd.watches.len(),
                    "rescan command"
                );

                if let Some(monitor) = coordinator.get_chain(cmd.chain_id).await {
                    if let Err(e) = monitor.rescan_from(cmd.from_block, cmd.watches).await {
                        error!(
                            chain_id = cmd.chain_id,
                            error = %e,
                            "rescan refused or failed"
                        );
                    }
                } else {
                    warn!(
                        chain_id = cmd.chain_id,
                        "chain not monitored by this instance"
                    );
                }
            }

            MonitorCommand::GetStatus(cmd) => {
                info!(chain_id = ?cmd.chain_id, "get status command");

                let chain_ids = if let Some(chain_id) = cmd.chain_id {
                    vec![chain_id]
                } else {
                    coordinator.chain_ids().await
                };

                for chain_id in chain_ids {
                    if let Some(monitor) = coordinator.get_chain(chain_id).await {
                        let watched = monitor.watched_addresses().await;
                        let current_block = monitor.current_block().await.unwrap_or(0);

                        let event = MonitorEvent::StatusReport(StatusReport {
                            chain_id,
                            watched_count: watched.len(),
                            current_block,
                            addresses: watched
                                .into_iter()
                                .map(|w| WatchedAddressInfo {
                                    address: w.address,
                                    invoice_id: w.invoice_id,
                                    token_contract: w.token_contract,
                                })
                                .collect(),
                            reported_at: Utc::now(),
                        });

                        if let Err(e) = bridge.publish(&event).await {
                            error!(error = %e, "failed to publish StatusReport event");
                        }
                    }
                }
            }
        }
    }

    warn!("commands stream ended");
}
