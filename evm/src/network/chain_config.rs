use super::EvmNetwork;
use serde::{Deserialize, Serialize};

/// Configuration for an EVM-compatible chain.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainConfig {
    /// The network identifier (None for testnets).
    pub network: Option<EvmNetwork>,
    /// EIP-155 chain ID.
    pub chain_id: u64,
    /// Human-readable chain name.
    pub name: &'static str,
    /// Native currency symbol (e.g., "ETH", "MATIC").
    pub native_symbol: &'static str,
    /// Native currency decimals (always 18 for EVM chains).
    pub native_decimals: u8,
    /// Average block time in seconds.
    pub block_time_secs: u64,
    /// Number of confirmations required for finality.
    pub confirmations_required: u32,
    /// Block explorer URL template (use {tx} for tx hash, {address} for address).
    pub explorer_url: &'static str,
}

impl ChainConfig {
    /// Get the explorer URL for a transaction.
    pub fn tx_explorer_url(&self, tx_hash: &str) -> String {
        self.explorer_url.replace("{tx}", tx_hash)
    }

    /// Get the explorer URL for an address.
    pub fn address_explorer_url(&self, address: &str) -> String {
        self.explorer_url
            .replace("/tx/{tx}", &format!("/address/{}", address))
    }

    /// Minimum time a confirmed payment's address should stay watched before
    /// `InvoiceCleanupService` is allowed to unwatch it.
    ///
    /// A reorg re-validates candidates by re-scanning currently watched
    /// addresses, so unwatching too soon after confirmation makes a
    /// relocated-but-still-paid transaction indistinguishable from a
    /// genuinely gone one. `confirmations_required * block_time_secs` is
    /// this chain's own estimate of how long it takes to reach the depth we
    /// already treat as final; a reorg reaching that deep *again* after
    /// confirmation is the tail event this floor exists to catch, so it
    /// here doubles it rather than pulling a chain-agnostic number out of
    /// thin air. This is a floor, not the actual grace period — callers
    /// combine it with their own configured value (typically much larger)
    /// via `max`, so it only bites for a chain whose confirmations/block
    /// time make the operator's flat default too thin.
    pub fn min_paid_unwatch_grace_period_secs(&self) -> u64 {
        (self.confirmations_required as u64)
            .saturating_mul(self.block_time_secs)
            .saturating_mul(2)
    }
}

/// Ethereum Mainnet configuration.
pub const ETHEREUM: ChainConfig = ChainConfig {
    network: Some(EvmNetwork::Ethereum),
    chain_id: 1,
    name: "Ethereum",
    native_symbol: "ETH",
    native_decimals: 18,
    block_time_secs: 12,
    confirmations_required: 12,
    explorer_url: "https://etherscan.io/tx/{tx}",
};

/// Polygon (formerly Matic) configuration.
pub const POLYGON: ChainConfig = ChainConfig {
    network: Some(EvmNetwork::Polygon),
    chain_id: 137,
    name: "Polygon",
    native_symbol: "POL",
    native_decimals: 18,
    block_time_secs: 2,
    confirmations_required: 128,
    explorer_url: "https://polygonscan.com/tx/{tx}",
};

/// Arbitrum One configuration.
pub const ARBITRUM: ChainConfig = ChainConfig {
    network: Some(EvmNetwork::Arbitrum),
    chain_id: 42161,
    name: "Arbitrum One",
    native_symbol: "ETH",
    native_decimals: 18,
    block_time_secs: 1,
    confirmations_required: 64,
    explorer_url: "https://arbiscan.io/tx/{tx}",
};

/// Optimism configuration.
pub const OPTIMISM: ChainConfig = ChainConfig {
    network: Some(EvmNetwork::Optimism),
    chain_id: 10,
    name: "Optimism",
    native_symbol: "ETH",
    native_decimals: 18,
    block_time_secs: 2,
    confirmations_required: 64,
    explorer_url: "https://optimistic.etherscan.io/tx/{tx}",
};

/// Base (Coinbase L2) configuration.
pub const BASE: ChainConfig = ChainConfig {
    network: Some(EvmNetwork::Base),
    chain_id: 8453,
    name: "Base",
    native_symbol: "ETH",
    native_decimals: 18,
    block_time_secs: 2,
    confirmations_required: 64,
    explorer_url: "https://basescan.org/tx/{tx}",
};

/// Avalanche C-Chain configuration.
pub const AVALANCHE: ChainConfig = ChainConfig {
    network: Some(EvmNetwork::Avalanche),
    chain_id: 43114,
    name: "Avalanche C-Chain",
    native_symbol: "AVAX",
    native_decimals: 18,
    block_time_secs: 2,
    confirmations_required: 12,
    explorer_url: "https://snowtrace.io/tx/{tx}",
};

/// BNB Smart Chain configuration.
pub const BINANCE_SMART_CHAIN: ChainConfig = ChainConfig {
    network: Some(EvmNetwork::BinanceSmartChain),
    chain_id: 56,
    name: "BNB Smart Chain",
    native_symbol: "BNB",
    native_decimals: 18,
    block_time_secs: 3,
    confirmations_required: 15,
    explorer_url: "https://bscscan.com/tx/{tx}",
};

/// zkSync Era configuration.
pub const ZKSYNC: ChainConfig = ChainConfig {
    network: Some(EvmNetwork::ZkSync),
    chain_id: 324,
    name: "zkSync Era",
    native_symbol: "ETH",
    native_decimals: 18,
    block_time_secs: 1,
    confirmations_required: 1, // ZK proofs provide finality
    explorer_url: "https://explorer.zksync.io/tx/{tx}",
};

/// Linea configuration.
pub const LINEA: ChainConfig = ChainConfig {
    network: Some(EvmNetwork::Linea),
    chain_id: 59144,
    name: "Linea",
    native_symbol: "ETH",
    native_decimals: 18,
    block_time_secs: 2,
    confirmations_required: 20,
    explorer_url: "https://lineascan.build/tx/{tx}",
};

/// Scroll configuration.
pub const SCROLL: ChainConfig = ChainConfig {
    network: Some(EvmNetwork::Scroll),
    chain_id: 534352,
    name: "Scroll",
    native_symbol: "ETH",
    native_decimals: 18,
    block_time_secs: 3,
    confirmations_required: 20,
    explorer_url: "https://scrollscan.com/tx/{tx}",
};

/// Fantom Opera configuration.
pub const FANTOM: ChainConfig = ChainConfig {
    network: Some(EvmNetwork::Fantom),
    chain_id: 250,
    name: "Fantom Opera",
    native_symbol: "FTM",
    native_decimals: 18,
    block_time_secs: 1,
    confirmations_required: 12,
    explorer_url: "https://ftmscan.com/tx/{tx}",
};

/// Gnosis Chain configuration.
pub const GNOSIS: ChainConfig = ChainConfig {
    network: Some(EvmNetwork::Gnosis),
    chain_id: 100,
    name: "Gnosis Chain",
    native_symbol: "xDAI",
    native_decimals: 18,
    block_time_secs: 5,
    confirmations_required: 20,
    explorer_url: "https://gnosisscan.io/tx/{tx}",
};

/// All supported chains.
pub const ALL_CHAINS: &[ChainConfig] = &[
    ETHEREUM,
    POLYGON,
    ARBITRUM,
    OPTIMISM,
    BASE,
    AVALANCHE,
    BINANCE_SMART_CHAIN,
    ZKSYNC,
    LINEA,
    SCROLL,
    FANTOM,
    GNOSIS,
];

/// Get chain configuration by network.
pub fn get_chain_config(network: EvmNetwork) -> &'static ChainConfig {
    ALL_CHAINS
        .iter()
        .find(|c| c.network == Some(network))
        .expect("all networks have configs")
}

/// Get chain configuration by chain ID.
pub fn get_chain_config_by_id(chain_id: u64) -> Option<&'static ChainConfig> {
    ALL_CHAINS.iter().find(|c| c.chain_id == chain_id)
}

/// Get chain configuration by chain ID, including testnets.
///
/// This function first checks mainnet chains, then falls back to testnets.
pub fn get_any_chain_config(chain_id: u64) -> Option<&'static ChainConfig> {
    // Try mainnets first
    if let Some(config) = get_chain_config_by_id(chain_id) {
        return Some(config);
    }
    // Fall back to testnets
    crate::testnet::get_testnet_config(chain_id)
}
