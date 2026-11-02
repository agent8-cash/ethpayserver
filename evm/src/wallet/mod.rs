//! HD wallet derivation for secp256k1 receiving addresses.
//!
//! This module implements BIP-32/BIP-44 hierarchical deterministic wallet derivation
//! for generating a unique receiving address per invoice.
//!
//! # Derivation Path
//!
//! BIP-44 with purpose 44' and the coin type of the key's chain family - 60'
//! for Ethereum, 195' for Tron (see [`crate::family`]):
//! ```text
//! m / 44' / coin' / account' / change / address_index
//! ```
//!
//! For payment addresses, we use:
//! - account = 0 (default account)
//! - change = 0 (external chain, for receiving)
//! - address_index = incrementing index per invoice
//!
//! The coin type is the load-bearing part, and it is *not* recoverable from
//! the account-level xpub a merchant registers. Everything here therefore
//! takes the family explicitly rather than assuming Ethereum - an assumption
//! that, applied to a Tron key, derives addresses the merchant's own wallet
//! will never show.

/// How many addresses a merchant is asked to compare by eye against another
/// wallet before trusting a registered key. Shared between the offline
/// `derive-xpub` tool's printed check addresses and the server's
/// `verification_addresses` on `POST /wallets` so the two count the same
/// thing from one source rather than two constants that could silently drift
/// apart.
pub const VERIFICATION_ADDRESS_COUNT: u32 = 3;

// Test-only: `tests.rs` and `xpub_encoding_tests.rs` reach these through `use super::*`.
#[cfg(test)]
use crate::family::ChainFamily;
#[cfg(test)]
use coins_bip32::{
    enc::{MainnetEncoder, XKeyEncoder},
    path::DerivationPath,
    prelude::*,
};
#[cfg(test)]
use coins_bip39::{English, Mnemonic};
#[cfg(test)]
use std::str::FromStr;

mod hd_wallet;
mod xpub_deriver;

pub use hd_wallet::*;
pub use xpub_deriver::*;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod xpub_encoding_tests;
