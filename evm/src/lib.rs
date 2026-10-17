//! EVM blockchain interaction layer for ethpayserver.
//!
//! This crate provides all the blockchain-related functionality needed for
//! processing payments on EVM-compatible networks (Ethereum, Polygon, Arbitrum, etc.).
//!
//! # Features
//!
//! - **Chain configurations** - Pre-defined settings for 12 EVM networks
//! - **HD wallet derivation** - BIP-32/44 address generation for payment invoices
//! - **RPC provider** - Alloy-based provider for blockchain interaction
//! - **ERC20 support** - Token registry and balance checking
//!
//! # Example
//!
//! ```ignore
//! use evm::{network, wallet::HdWallet, provider::EvmProvider};
//!
//! // Create a wallet from mnemonic
//! let wallet = HdWallet::from_mnemonic("your mnemonic here", "")?;
//!
//! // Derive payment addresses
//! let address = wallet.derive_address(0)?;
//!
//! // Connect to Ethereum
//! let provider = EvmProvider::new(&network::ETHEREUM, "https://eth.llamarpc.com").await?;
//!
//! // Check balance
//! let balance = provider.get_native_balance(address).await?;
//! ```

#[cfg(feature = "api")]
pub mod api;

pub mod error;
pub mod family;
pub mod metrics;
pub mod monitor;
pub mod network;
pub mod provider;
#[cfg(feature = "sentry-scrub")]
pub mod telemetry;
pub mod testnet;
pub mod tokens;
// Reserved for a future hot-wallet mode: the server is non-custodial by
// design and holds no spending key today, so nothing turns this feature on.
// A caller that wants to move funds from a derived address has to opt into
// that as a deliberate, visible product decision rather than finding the
// capability already compiled in.
#[cfg(feature = "hot-wallet")]
pub mod transaction;
pub mod wallet;

// Re-export commonly used items
pub use alloy::providers::RootProvider;
pub use error::{EvmError, EvmResult};
pub use family::{
    ChainFamily, ETH_COIN_TYPE, NAMESPACE_EIP155, NAMESPACE_TRON, TRON_COIN_TYPE,
    family_for_namespace,
};
pub use network::{
    ALL_CHAINS, ChainConfig, EvmNetwork, get_any_chain_config, get_chain_config,
    get_chain_config_by_id,
};
pub use provider::EvmProvider;
pub use tokens::{EvmTokenStandard, Token, discover_token, get_token_balance, get_token_info};
pub use wallet::{
    HdWallet, VERIFICATION_ADDRESS_COUNT, XpubDeriver, generate_mnemonic, looks_like_a_private_key,
    validate_mnemonic, validate_xpub, xpub_depth_problem,
};

// Re-export alloy primitives that users will commonly need
pub use alloy::primitives::{Address, B256, U256};
