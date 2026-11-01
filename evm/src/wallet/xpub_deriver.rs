use super::hd_wallet::public_key_to_address;
use crate::error::{EvmError, EvmResult};
use crate::family::ChainFamily;
use alloy::primitives::Address;
use coins_bip32::{
    enc::{MainnetEncoder, XKeyEncoder},
    prelude::*,
};

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

/// Version bytes of the SLIP-132 single-sig public keys this converts.
const XPUB_VERSION: [u8; 4] = [0x04, 0x88, 0xb2, 0x1e];
const YPUB_VERSION: [u8; 4] = [0x04, 0x9d, 0x7c, 0xb2]; // BIP-49, P2SH-P2WPKH
const ZPUB_VERSION: [u8; 4] = [0x04, 0xb2, 0x47, 0x46]; // BIP-84, P2WPKH

/// Rewrites a BIP-49 `ypub` or BIP-84 `zpub` account key as the `xpub` with the
/// same key material, so one canonical form is stored. Matched on the version
/// bytes, not the prefix text, so SLIP-132 multisig keys (`Ypub`/`Zpub`, other
/// bytes) and anything else fall through as `None` and are never reinterpreted.
///
/// Returns `None` for an xpub too: it is already the stored form.
pub fn xpub_from_slip132(key_str: &str) -> Option<String> {
    // The decoder panics on a payload shorter than its 4-byte checksum, so
    // refuse short input here, as `validate_xpub` does.
    if key_str.len() < 10 {
        return None;
    }
    let mut payload = coins_bip32::enc::decode_b58_check(key_str).ok()?;
    let version: [u8; 4] = payload.get(..4)?.try_into().ok()?;
    if version != YPUB_VERSION && version != ZPUB_VERSION {
        return None;
    }
    payload[..4].copy_from_slice(&XPUB_VERSION);
    Some(coins_bip32::enc::encode_b58_check(&payload))
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
