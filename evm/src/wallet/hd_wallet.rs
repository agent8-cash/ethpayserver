//! BIP-39 mnemonic and HD (BIP-32) key derivation for receiving addresses.

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

/// HD wallet for deriving Ethereum addresses.
#[derive(Clone)]
pub struct HdWallet {
    /// The master extended private key.
    pub(super) master_key: XPriv,
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
pub(super) fn public_key_to_address(public_key: &XPub) -> Address {
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
