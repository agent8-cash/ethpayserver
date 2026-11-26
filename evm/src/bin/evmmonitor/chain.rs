//! Chain monitor construction from RPC configuration.

use std::sync::Arc;

use evm::error::{EvmError, EvmResult};
use evm::monitor::{ChainMonitor, ChainMonitorConfig, RpcBlockSource};
use evm::network::get_any_chain_config;

use super::config::ChainRpcConfig;

pub(crate) async fn create_chain_monitor(
    rpc_config: &ChainRpcConfig,
) -> EvmResult<Arc<ChainMonitor<RpcBlockSource>>> {
    use evm::monitor::RpcSourceConfig;
    use secrecy::ExposeSecret;

    let chain_config = get_any_chain_config(rpc_config.chain_id)
        .ok_or_else(|| EvmError::Monitor(format!("unknown chain id: {}", rpc_config.chain_id)))?;

    let source_config = match &rpc_config.rpc_ws {
        Some(ws_url) => RpcSourceConfig::with_websocket(
            ws_url.expose_secret(),
            rpc_config.rpc_http.expose_secret(),
            rpc_config.chain_id,
        ),
        None => {
            RpcSourceConfig::http_only(rpc_config.rpc_http.expose_secret(), rpc_config.chain_id)
        }
    }
    .with_http_fallbacks(
        rpc_config
            .rpc_http_fallbacks
            .iter()
            .map(|url| url.expose_secret().to_string()),
    );

    let source = RpcBlockSource::new(source_config).await?;
    let monitor_config = ChainMonitorConfig::from_chain(chain_config);
    let monitor = ChainMonitor::new(chain_config, source, monitor_config);

    Ok(Arc::new(monitor))
}
