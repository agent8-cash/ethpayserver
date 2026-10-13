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

use crate::error::{EvmError, EvmResult};
use crate::family::ChainFamily;
use alloy::primitives::Address;
use coins_bip32::{
    enc::{MainnetEncoder, XKeyEncoder},
    path::DerivationPath,
    prelude::*,
};
use coins_bip39::{English, Mnemonic};
use std::str::FromStr;

/// How many addresses a merchant is asked to compare by eye against another
/// wallet before trusting a registered key. Shared between the offline
/// `derive-xpub` tool's printed check addresses and the server's
/// `verification_addresses` on `POST /wallets` so the two count the same
/// thing from one source rather than two constants that could silently drift
/// apart.
pub const VERIFICATION_ADDRESS_COUNT: u32 = 3;

/// HD wallet for deriving Ethereum addresses.
#[derive(Clone)]
pub struct HdWallet {
    /// The master extended private key.
    master_key: XPriv,
}

impl HdWallet {
    /// Create a new HD wallet from a BIP-39 mnemonic phrase.
    ///
    /// # Arguments
    ///
    /// * `mnemonic` - A valid BIP-39 mnemonic (12, 15, 18, 21, or 24 words)
    /// * `passphrase` - Optional passphrase for additional security (empty string if none)
    ///
    /// # Example
    ///
    /// ```ignore
    /// let wallet = HdWallet::from_mnemonic(
    ///     "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about",
    ///     ""
    /// )?;
    /// ```
    pub fn from_mnemonic(mnemonic: &str, passphrase: &str) -> EvmResult<Self> {
        let mnemonic = Mnemonic::<English>::new_from_phrase(mnemonic)
            .map_err(|e| EvmError::InvalidMnemonic(e.to_string()))?;

        let seed = mnemonic
            .to_seed(Some(passphrase))
            .map_err(|e| EvmError::InvalidMnemonic(format!("failed to derive seed: {}", e)))?;

        // `root_from_seed(_, None)` defaults to `Hint::SegWit`, which would
        // render an account xpub with BIP-84 ("zpub") version bytes instead
        // of the BIP-44 ("xpub") ones every merchant wallet and this crate's
        // own docs mean by "xpub". The hint changes nothing about derivation
        // - addresses come out identical either way - only how a key
        // encodes to base58, so this is spelled out explicitly rather than
        // left to the library default.
        let master_key = XPriv::root_from_seed(&seed[..], Some(Hint::Legacy))
            .map_err(|e| EvmError::WalletDerivation(e.to_string()))?;

        Ok(Self { master_key })
    }

    /// Create a new HD wallet from raw seed bytes (64 bytes).
    pub fn from_seed(seed: &[u8]) -> EvmResult<Self> {
        if seed.len() != 64 {
            return Err(EvmError::WalletDerivation(format!(
                "seed must be 64 bytes, got {}",
                seed.len()
            )));
        }

        let master_key = XPriv::root_from_seed(seed, Some(Hint::Legacy))
            .map_err(|e| EvmError::WalletDerivation(e.to_string()))?;

        Ok(Self { master_key })
    }

    /// Derive an Ethereum address at the given BIP-44 index.
    ///
    /// Uses path: m/44'/60'/0'/0/{index}
    ///
    /// # Arguments
    ///
    /// * `index` - The address index (0, 1, 2, ...)
    ///
    /// # Returns
    ///
    /// The derived Ethereum address as a checksummed string.
    pub fn derive_address(&self, index: u32) -> EvmResult<Address> {
        self.derive_address_for(ChainFamily::Evm, index)
    }

    /// Derive the receiving address bytes for one chain family at `index`.
    ///
    /// The family chooses the coin type, so the same seed produces entirely
    /// different keys - and therefore entirely different addresses - for
    /// Ethereum and for Tron. That is the property that makes the two
    /// non-interchangeable, and re-encoding one family's address in the
    /// other's alphabet does not produce the other's address.
    ///
    /// Returns raw address bytes; render them with
    /// [`ChainFamily::encode_address`].
    pub fn derive_address_for(&self, family: ChainFamily, index: u32) -> EvmResult<Address> {
        self.derive_address_at_path(&family.derivation_path(index))
    }

    /// Derive an Ethereum address at a custom derivation path.
    ///
    /// # Arguments
    ///
    /// * `path` - BIP-32 derivation path (e.g., "m/44'/60'/0'/0/0")
    pub fn derive_address_at_path(&self, path: &str) -> EvmResult<Address> {
        let derivation_path = DerivationPath::from_str(path)
            .map_err(|e| EvmError::InvalidDerivationPath(e.to_string()))?;

        let derived_key = self
            .master_key
            .derive_path(&derivation_path)
            .map_err(|e| EvmError::WalletDerivation(e.to_string()))?;

        let public_key = derived_key.verify_key();
        let address = public_key_to_address(&public_key);

        Ok(address)
    }

    /// Derive multiple addresses starting from the given index.
    ///
    /// # Arguments
    ///
    /// * `start_index` - Starting index
    /// * `count` - Number of addresses to derive
    pub fn derive_addresses(&self, start_index: u32, count: u32) -> EvmResult<Vec<Address>> {
        let mut addresses = Vec::with_capacity(count as usize);
        for i in start_index..start_index + count {
            addresses.push(self.derive_address(i)?);
        }
        Ok(addresses)
    }

    /// Derive the private key at the given BIP-44 index.
    ///
    /// Uses path: m/44'/60'/0'/0/{index}
    ///
    /// # Security
    ///
    /// The returned key bytes must be handled securely and zeroized after use.
    /// Never log or persist private keys.
    pub fn derive_private_key(&self, index: u32) -> EvmResult<[u8; 32]> {
        let path = format!("m/44'/60'/0'/0/{}", index);
        let derivation_path = DerivationPath::from_str(&path)
            .map_err(|e| EvmError::InvalidDerivationPath(e.to_string()))?;

        let derived_key = self
            .master_key
            .derive_path(&derivation_path)
            .map_err(|e| EvmError::WalletDerivation(e.to_string()))?;

        let signing_key: &k256::ecdsa::SigningKey = derived_key.as_ref();
        let key_bytes = signing_key.to_bytes();
        let mut result = [0u8; 32];
        result.copy_from_slice(&key_bytes);
        Ok(result)
    }

    /// Get the extended public key for account 0.
    ///
    /// This can be used to derive addresses without the private key.
    pub fn account_xpub(&self) -> EvmResult<XPub> {
        self.account_xpub_for(ChainFamily::Evm)
    }

    /// The account-level extended public key a merchant's wallet would export
    /// for one chain family - `m/44'/60'/0'` or `m/44'/195'/0'`.
    ///
    /// The two are byte-indistinguishable in form: same version bytes, same
    /// base58 alphabet, same length. Only the key material differs, and
    /// nothing can read the coin type back out of it. That is why the family
    /// is stored beside the key rather than inferred from it.
    pub fn account_xpub_for(&self, family: ChainFamily) -> EvmResult<XPub> {
        let path = DerivationPath::from_str(&family.account_path())
            .map_err(|e| EvmError::InvalidDerivationPath(e.to_string()))?;

        let account_key = self
            .master_key
            .derive_path(&path)
            .map_err(|e| EvmError::WalletDerivation(e.to_string()))?;

        Ok(account_key.verify_key())
    }

    /// [`Self::account_xpub_for`], base58-encoded - the exact string a
    /// merchant pastes into `POST /wallets`.
    pub fn account_xpub_string_for(&self, family: ChainFamily) -> EvmResult<String> {
        MainnetEncoder::xpub_to_base58(&self.account_xpub_for(family)?)
            .map_err(|e| EvmError::WalletDerivation(e.to_string()))
    }
}

/// Convert a secp256k1 public key to an Ethereum address.
///
/// Ethereum addresses are the last 20 bytes of the Keccak-256 hash
/// of the uncompressed public key (without the 0x04 prefix).
fn public_key_to_address(public_key: &XPub) -> Address {
    use alloy::primitives::keccak256;
    use k256::PublicKey;
    use k256::elliptic_curve::sec1::ToEncodedPoint;

    // Get compressed SEC1 bytes from XPub
    let compressed_bytes = public_key.to_sec1_bytes();

    // Parse as k256 PublicKey and convert to uncompressed format
    let k256_pubkey =
        PublicKey::from_sec1_bytes(&compressed_bytes).expect("valid public key from XPub");
    let encoded = k256_pubkey.to_encoded_point(false); // false = uncompressed
    let pubkey_bytes = encoded.as_bytes();

    // Skip the 0x04 prefix and hash the 64 remaining bytes
    let hash = keccak256(&pubkey_bytes[1..]);

    // Take the last 20 bytes
    Address::from_slice(&hash[12..])
}

/// Generate a new random mnemonic phrase.
///
/// # Arguments
///
/// * `word_count` - Number of words (12, 15, 18, 21, or 24)
pub fn generate_mnemonic(word_count: usize) -> EvmResult<String> {
    use coins_bip39::Entropy;
    use rand::Rng;

    let entropy_bytes = match word_count {
        12 => 16,
        15 => 20,
        18 => 24,
        21 => 28,
        24 => 32,
        _ => {
            return Err(EvmError::InvalidMnemonic(format!(
                "invalid word count: {}, must be 12, 15, 18, 21, or 24",
                word_count
            )));
        }
    };

    let mut bytes = vec![0u8; entropy_bytes];
    rand::rng().fill(&mut bytes[..]);

    let entropy =
        Entropy::from_slice(&bytes).map_err(|e| EvmError::InvalidMnemonic(e.to_string()))?;

    let mnemonic = Mnemonic::<English>::new_from_entropy(entropy);

    Ok(mnemonic.to_phrase())
}

/// Validate a mnemonic phrase.
pub fn validate_mnemonic(phrase: &str) -> bool {
    Mnemonic::<English>::new_from_phrase(phrase).is_ok()
}

/// Address deriver from an extended public key, bound to a chain family.
///
/// Allows deriving payment addresses without access to private keys.
/// Merchants provide their xpub and the server derives unique addresses
/// for each invoice.
///
/// The family is carried, not guessed. An account xpub has its coin type
/// already spent - the deriver only walks `0/{index}` below it - so by the
/// time a key reaches here, which family it belongs to is no longer a
/// question this code could answer from the bytes. Every constructor
/// therefore demands it, and [`Self::derive_evm_address`] refuses to hand out
/// EVM-typed bytes for a key that was not registered as EVM.
#[derive(Clone)]
pub struct XpubDeriver {
    /// The account-level extended public key (at path m/44'/{coin}'/0')
    account_xpub: XPub,
    /// The family the key was registered for. Decides how an address is
    /// rendered, and which callers may use it at all.
    family: ChainFamily,
}

impl XpubDeriver {
    /// Create a deriver for a key registered under `namespace`.
    ///
    /// The namespace comes from the wallet row the key was read from - never
    /// from the key, which cannot be asked, and never from a default. An
    /// unknown namespace is an error rather than a fallback to Ethereum.
    pub fn from_xpub(namespace: &str, xpub_str: &str) -> EvmResult<Self> {
        Self::for_family(crate::family::family_for_namespace(namespace)?, xpub_str)
    }

    /// Create a deriver for a known family.
    pub fn for_family(family: ChainFamily, xpub_str: &str) -> EvmResult<Self> {
        let account_xpub = MainnetEncoder::xpub_from_base58(xpub_str)
            .map_err(|e| EvmError::InvalidXpub(format!("failed to parse xpub: {}", e)))?;

        Ok(Self {
            account_xpub,
            family,
        })
    }

    /// The family this key was registered for.
    pub fn family(&self) -> ChainFamily {
        self.family
    }

    /// The full BIP-44 path of the address at `index`, coin type included.
    pub fn derivation_path(&self, index: u32) -> String {
        self.family.derivation_path(index)
    }

    /// Derive the receiving address at `index`, rendered in this key's own
    /// family encoding.
    ///
    /// `0x…` for `eip155`, `T…` for `tron`. This is what a merchant compares
    /// against their own wallet, and what a customer is asked to pay.
    pub fn derive_address(&self, index: u32) -> EvmResult<String> {
        Ok(self
            .family
            .encode_address(self.derive_address_bytes(index)?))
    }

    /// Derive the address at `index` as EVM address bytes.
    ///
    /// Refuses a key registered for any other family. The bytes are the same
    /// shape whatever the family, which is exactly why this refuses rather
    /// than converts: an `Address` flows on into the chain monitor and the
    /// watched-address table, both of which mean *an EVM address on an EVM
    /// chain*, and a Tron key's bytes there would have the server watching an
    /// Ethereum address for a payment that is never coming.
    pub fn derive_evm_address(&self, index: u32) -> EvmResult<Address> {
        if self.family != ChainFamily::Evm {
            return Err(EvmError::InvalidXpub(format!(
                "this key is registered for `{}`, not an EVM chain; it cannot                  derive an EVM address",
                self.family.namespace()
            )));
        }
        self.derive_address_bytes(index)
    }

    /// Walk `0/{index}` below the account key. Family-independent: the two
    /// families differ in the coin type already spent above this point, and
    /// in nothing below it.
    fn derive_address_bytes(&self, index: u32) -> EvmResult<Address> {
        // Derive external chain (0) then index
        let external = self
            .account_xpub
            .derive_child(0)
            .map_err(|e| EvmError::WalletDerivation(e.to_string()))?;

        let derived = external
            .derive_child(index)
            .map_err(|e| EvmError::WalletDerivation(e.to_string()))?;

        Ok(public_key_to_address(&derived))
    }

    /// Get the xpub as a base58-encoded string.
    pub fn xpub_string(&self) -> EvmResult<String> {
        MainnetEncoder::xpub_to_base58(&self.account_xpub)
            .map_err(|e| EvmError::WalletDerivation(e.to_string()))
    }
}

/// Validate an extended public key string.
pub fn validate_xpub(xpub_str: &str) -> bool {
    if xpub_str.is_empty() || xpub_str.len() < 10 {
        return false;
    }
    MainnetEncoder::xpub_from_base58(xpub_str).is_ok()
}

/// BIP-32 depth of the key the deriver expects: `m/44'/{coin}'/0'` is three
/// derivations below the master key.
pub const ACCOUNT_XPUB_DEPTH: u8 = 3;

/// If `xpub_str` parses but is not at the account depth, say so.
///
/// Derivation appends `0/{index}` to whatever was registered, so a key one
/// level too deep (the receive branch, `m/44'/60'/0'/0`) yields addresses at
/// `.../0/0/{index}` - valid, owned by the merchant, and never shown by their
/// wallet's standard scan. Nothing errors, so the key is refused up front
/// with the depth named. A string that does not parse returns `None`; that
/// is `validate_xpub`'s refusal to make.
///
/// A dedicated hardened account such as `m/44'/60'/7'` is also depth 3 and
/// is accepted.
pub fn xpub_depth_problem(xpub_str: &str) -> Option<String> {
    let xpub = MainnetEncoder::xpub_from_base58(xpub_str).ok()?;
    let info: &coins_bip32::primitives::XKeyInfo = xpub.as_ref();
    let depth = info.depth;
    if depth == ACCOUNT_XPUB_DEPTH {
        return None;
    }
    Some(format!(
        "invalid_xpub_depth: this key is at depth {depth}; register the account-level key at \
         depth {ACCOUNT_XPUB_DEPTH} (the m/44'/<coin>'/<account>' level your wallet exports, \
         e.g. m/44'/60'/0' for Ethereum), not a deeper or shallower one"
    ))
}

/// Names the more likely reason a string failed `validate_xpub`: it looks
/// like a pasted *private* key rather than a typo or truncation - either an
/// extended private key (`xprv`/`tprv`) or the raw 32-byte secp256k1 key a
/// wallet like MetaMask exports as `0x`-prefixed hex.
///
/// `validate_xpub` already refuses this on the version byte (for the `xprv`
/// case) or because it isn't base58 at all (for raw hex) - so this takes no
/// part in that boundary. It exists only so a caller can tell an operator
/// what they actually pasted instead of a bare refusal, which matters
/// because the more specific message is what stops someone from trying the
/// same private key again with a typo "fixed".
pub fn looks_like_a_private_key(xpub_str: &str) -> bool {
    let trimmed = xpub_str.trim();
    if trimmed.starts_with("xprv") || trimmed.starts_with("tprv") {
        return true;
    }
    let hex = trimmed.strip_prefix("0x").unwrap_or(trimmed);
    hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod xpub_encoding_tests;
