//! EVM chain configurations and traits.
//!
//! This module defines the supported EVM chains and their configurations.
//! Each chain has specific parameters like chain ID, RPC endpoints, block time, etc.

use serde::{Deserialize, Serialize};

/// Supported EVM-compatible networks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvmNetwork {
    /// Ethereum mainnet
    Ethereum,
    /// Polygon (formerly Matic)
    Polygon,
    /// Arbitrum One
    Arbitrum,
    /// Optimism
    Optimism,
    /// Base (Coinbase L2)
    Base,
    /// Avalanche C-Chain
    Avalanche,
    /// BNB Smart Chain (formerly BSC)
    BinanceSmartChain,
    /// zkSync Era
    ZkSync,
    /// Linea
    Linea,
    /// Scroll
    Scroll,
    /// Fantom Opera
    Fantom,
    /// Gnosis (formerly xDai)
    Gnosis,
}

impl EvmNetwork {
    /// Returns the display name for this network.
    pub fn display_name(&self) -> &'static str {
        match self {
            EvmNetwork::Ethereum => "Ethereum",
            EvmNetwork::Polygon => "Polygon",
            EvmNetwork::Arbitrum => "Arbitrum",
            EvmNetwork::Optimism => "Optimism",
            EvmNetwork::Base => "Base",
            EvmNetwork::Avalanche => "Avalanche",
            EvmNetwork::BinanceSmartChain => "BNB Chain",
            EvmNetwork::ZkSync => "zkSync",
            EvmNetwork::Linea => "Linea",
            EvmNetwork::Scroll => "Scroll",
            EvmNetwork::Fantom => "Fantom",
            EvmNetwork::Gnosis => "Gnosis",
        }
    }

    /// Returns the native currency symbol for this network.
    pub fn native_symbol(&self) -> &'static str {
        match self {
            EvmNetwork::Ethereum => "ETH",
            EvmNetwork::Polygon => "POL",
            EvmNetwork::Arbitrum => "ETH",
            EvmNetwork::Optimism => "ETH",
            EvmNetwork::Base => "ETH",
            EvmNetwork::Avalanche => "AVAX",
            EvmNetwork::BinanceSmartChain => "BNB",
            EvmNetwork::ZkSync => "ETH",
            EvmNetwork::Linea => "ETH",
            EvmNetwork::Scroll => "ETH",
            EvmNetwork::Fantom => "FTM",
            EvmNetwork::Gnosis => "xDAI",
        }
    }

    /// Returns the EIP-155 chain ID for this network.
    pub fn chain_id(&self) -> u64 {
        match self {
            EvmNetwork::Ethereum => 1,
            EvmNetwork::Polygon => 137,
            EvmNetwork::Arbitrum => 42161,
            EvmNetwork::Optimism => 10,
            EvmNetwork::Base => 8453,
            EvmNetwork::Avalanche => 43114,
            EvmNetwork::BinanceSmartChain => 56,
            EvmNetwork::ZkSync => 324,
            EvmNetwork::Linea => 59144,
            EvmNetwork::Scroll => 534352,
            EvmNetwork::Fantom => 250,
            EvmNetwork::Gnosis => 100,
        }
    }

    /// Get network from chain ID.
    pub fn from_chain_id(chain_id: u64) -> Option<Self> {
        match chain_id {
            1 => Some(EvmNetwork::Ethereum),
            137 => Some(EvmNetwork::Polygon),
            42161 => Some(EvmNetwork::Arbitrum),
            10 => Some(EvmNetwork::Optimism),
            8453 => Some(EvmNetwork::Base),
            43114 => Some(EvmNetwork::Avalanche),
            56 => Some(EvmNetwork::BinanceSmartChain),
            324 => Some(EvmNetwork::ZkSync),
            59144 => Some(EvmNetwork::Linea),
            534352 => Some(EvmNetwork::Scroll),
            250 => Some(EvmNetwork::Fantom),
            100 => Some(EvmNetwork::Gnosis),
            _ => None,
        }
    }

    /// Returns the network identifier as a string.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ethereum => "ethereum",
            Self::Polygon => "polygon",
            Self::Arbitrum => "arbitrum",
            Self::Optimism => "optimism",
            Self::Base => "base",
            Self::Avalanche => "avalanche",
            Self::BinanceSmartChain => "binance_smart_chain",
            Self::ZkSync => "zksync",
            Self::Linea => "linea",
            Self::Scroll => "scroll",
            Self::Fantom => "fantom",
            Self::Gnosis => "gnosis",
        }
    }
}

impl std::fmt::Display for EvmNetwork {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for EvmNetwork {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "ethereum" | "eth" => Ok(Self::Ethereum),
            "polygon" | "matic" => Ok(Self::Polygon),
            "arbitrum" | "arb" => Ok(Self::Arbitrum),
            "optimism" | "op" => Ok(Self::Optimism),
            "base" => Ok(Self::Base),
            "avalanche" | "avax" => Ok(Self::Avalanche),
            "binance_smart_chain" | "bsc" | "bnb" => Ok(Self::BinanceSmartChain),
            "zksync" | "zk_sync" => Ok(Self::ZkSync),
            "linea" => Ok(Self::Linea),
            "scroll" => Ok(Self::Scroll),
            "fantom" | "ftm" => Ok(Self::Fantom),
            "gnosis" | "xdai" => Ok(Self::Gnosis),
            _ => Err(format!("unknown network: {}", s)),
        }
    }
}

mod chain_config;
pub use chain_config::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_chain_config() {
        let config = get_chain_config(EvmNetwork::Ethereum);
        assert_eq!(config.chain_id, 1);
        assert_eq!(config.native_symbol, "ETH");
    }

    #[test]
    fn test_get_chain_config_by_id() {
        let config = get_chain_config_by_id(137).unwrap();
        assert_eq!(config.network, Some(EvmNetwork::Polygon));
        assert_eq!(config.native_symbol, "POL");
    }

    #[test]
    fn test_network_from_chain_id() {
        assert_eq!(EvmNetwork::from_chain_id(1), Some(EvmNetwork::Ethereum));
        assert_eq!(EvmNetwork::from_chain_id(137), Some(EvmNetwork::Polygon));
        assert_eq!(EvmNetwork::from_chain_id(999999), None);
    }

    #[test]
    fn test_explorer_url() {
        let tx_url = ETHEREUM.tx_explorer_url("0x123abc");
        assert_eq!(tx_url, "https://etherscan.io/tx/0x123abc");
    }

    #[test]
    fn test_all_chains_have_unique_ids() {
        let mut chain_ids: Vec<u64> = ALL_CHAINS.iter().map(|c| c.chain_id).collect();
        chain_ids.sort();
        chain_ids.dedup();
        assert_eq!(chain_ids.len(), ALL_CHAINS.len());
    }

    #[test]
    fn test_network_chain_id_matches_config() {
        for config in ALL_CHAINS {
            if let Some(network) = config.network {
                assert_eq!(network.chain_id(), config.chain_id);
            }
        }
    }

    /// Every mainnet that has tokens seeded in the `tokens` table must be a
    /// real member of `ALL_CHAINS` - the same check `evm::testnet` runs for
    /// the newly-seeded testnets, mirrored here so both directions are
    /// covered instead of just the one the original PR happened to add.
    /// Named by id, not iterated from `ALL_CHAINS`, so this fails the moment
    /// either list drifts from the other instead of trivially agreeing with
    /// itself.
    #[test]
    fn test_seeded_mainnets_are_all_offered() {
        for (chain_id, name) in [(324, "zkSync Era"), (59144, "Linea"), (534352, "Scroll")] {
            let config = get_chain_config_by_id(chain_id)
                .unwrap_or_else(|| panic!("{name} ({chain_id}) is not in ALL_CHAINS"));
            assert_eq!(config.name, name);
        }
    }

    #[test]
    fn test_min_paid_unwatch_grace_period_is_double_the_confirmation_time() {
        // Polygon: 128 confirmations * 2s/block * 2 = 512s.
        assert_eq!(POLYGON.min_paid_unwatch_grace_period_secs(), 512);
        // zkSync: 1 confirmation * 1s/block * 2 = 2s — a much smaller floor,
        // since its single confirmation is backed by a ZK validity proof
        // rather than probabilistic depth; the flat operator default (not
        // computed here) is what actually protects it.
        assert_eq!(ZKSYNC.min_paid_unwatch_grace_period_secs(), 2);
    }
}
