//! Which chain family a receiving key belongs to, and everything that decides.
//!
//! A merchant hands this server an *account-level* extended public key -
//! `m/44'/<coin>'/0'`. The BIP-44 coin type is already baked into it and the
//! parent is unreachable, so the key cannot be asked which family it came
//! from. Worse, it cannot be told apart by looking: an Ethereum account xpub
//! and a Tron one are byte-indistinguishable - same base58 alphabet, same
//! `0x0488B21E` version bytes, same length. A merchant pasting into the wrong
//! field gets no feedback, and neither does the server.
//!
//! What the family decides is not cosmetic:
//!
//! | family   | namespace | coin type | address                       |
//! |----------|-----------|-----------|-------------------------------|
//! | EVM      | `eip155`  | 60        | `0x…` EIP-55 checksummed hex  |
//! | Tron     | `tron`    | 195       | `T…` base58check, `0x41` tag  |
//!
//! Both are secp256k1 and both take the last 20 bytes of the Keccak-256 hash
//! of the uncompressed public key, so the *bytes* of an address are produced
//! identically. Only the coin type and the text encoding differ - which is
//! exactly why getting it wrong is silent. Run an Ethereum account xpub
//! through Tron's encoding and out comes a valid, checksum-correct `T…`
//! address. Money sent there arrives. The merchant's Tron wallet never shows
//! it, because that wallet derives at `m/44'/195'`, and recovering it means
//! re-importing the seed at a non-standard path.
//!
//! So a family is declared when a key is registered and carried everywhere
//! that key goes, rather than guessed at from the key or from the chain a
//! payment happens to be quoted on.

use alloy::primitives::Address;

use crate::error::{EvmError, EvmResult};

/// CAIP-2 namespace for EVM chains.
///
/// Spelled here as well as in `types` so this crate does not depend on that
/// one; the pair is asserted equal by a test in `server`, which sees both.
pub const NAMESPACE_EIP155: &str = "eip155";
/// CAIP-2 namespace for Tron.
pub const NAMESPACE_TRON: &str = "tron";

/// Standard BIP-44 coin type for Ethereum (SLIP-44).
pub const ETH_COIN_TYPE: u32 = 60;
/// Standard BIP-44 coin type for Tron (SLIP-44).
pub const TRON_COIN_TYPE: u32 = 195;

/// The version byte Tron prefixes an address payload with before base58check.
/// `0x41` is what makes every mainnet Tron address start with `T`.
const TRON_ADDRESS_PREFIX: u8 = 0x41;

/// A family of chains that share a key derivation and an address encoding.
///
/// Deliberately closed, unlike [`crate::ChainId`]'s open namespace: a family
/// cannot be added by configuration, because adding one means writing the code
/// that derives and encodes for it. An unknown namespace is refused rather
/// than defaulted - defaulting is the bug this type exists to make
/// unrepresentable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChainFamily {
    /// Ethereum and every EIP-155 chain.
    Evm,
    /// Tron.
    Tron,
}

impl ChainFamily {
    /// The family a CAIP-2 namespace belongs to, or `None` if this build has
    /// no derivation for it.
    ///
    /// Takes the namespace rather than a whole chain id on purpose. Every
    /// `eip155:*` chain shares one key tree; the reference selects a network,
    /// not a derivation.
    pub fn from_namespace(namespace: &str) -> Option<Self> {
        match namespace {
            NAMESPACE_EIP155 => Some(Self::Evm),
            NAMESPACE_TRON => Some(Self::Tron),
            _ => None,
        }
    }

    /// The CAIP-2 namespace this family is named by.
    pub fn namespace(self) -> &'static str {
        match self {
            Self::Evm => NAMESPACE_EIP155,
            Self::Tron => NAMESPACE_TRON,
        }
    }

    /// The SLIP-44 coin type a merchant's wallet exported the key under.
    pub fn coin_type(self) -> u32 {
        match self {
            Self::Evm => ETH_COIN_TYPE,
            Self::Tron => TRON_COIN_TYPE,
        }
    }

    /// The account-level path a key for this family is exported from.
    pub fn account_path(self) -> String {
        format!("m/44'/{}'/0'", self.coin_type())
    }

    /// The full path of one receiving address, coin type included.
    ///
    /// Shown to the merchant beside the address. The coin type is the part
    /// worth reading: it is the only thing in the path that differs between
    /// families, and the only thing their own wallet has to agree with.
    pub fn derivation_path(self, index: u32) -> String {
        format!("{}/0/{}", self.account_path(), index)
    }

    /// Render an address in this family's own text encoding.
    ///
    /// The 20 bytes are the same for both families; this is the step that
    /// makes them readable by the right wallet - and, applied to bytes derived
    /// under the wrong coin type, the step that produces a perfectly valid
    /// address nobody can spend from.
    pub fn encode_address(self, address: Address) -> String {
        match self {
            Self::Evm => address.to_string(),
            Self::Tron => encode_tron_address(address),
        }
    }
}

/// Base58check-encode 20 address bytes the way Tron does.
///
/// `base58(0x41 ‖ address ‖ sha256(sha256(0x41 ‖ address))[..4])`. The
/// double-SHA256 checksum is Bitcoin's, not Keccak - a Tron address that a
/// wallet rejects as mistyped is one whose last four bytes disagree with the
/// rest, so this is what makes a typo in a copied address detectable at all.
fn encode_tron_address(address: Address) -> String {
    use sha2::{Digest, Sha256};

    let mut payload = Vec::with_capacity(25);
    payload.push(TRON_ADDRESS_PREFIX);
    payload.extend_from_slice(address.as_slice());

    let checksum = Sha256::digest(Sha256::digest(&payload));
    payload.extend_from_slice(&checksum[..4]);

    bs58::encode(payload).into_string()
}

/// Resolve a namespace to a family, or say which one it was.
///
/// The error names the namespace because the caller's own message cannot: it
/// is holding a chain id or a database row, and "unsupported namespace" with
/// nothing after it has sent people looking in the wrong place before.
pub fn family_for_namespace(namespace: &str) -> EvmResult<ChainFamily> {
    ChainFamily::from_namespace(namespace).ok_or_else(|| {
        EvmError::InvalidXpub(format!(
            "no key derivation is implemented for chain namespace `{namespace}`"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    /// The coin types are the published SLIP-44 values, and they differ.
    ///
    /// Worth asserting literally rather than reading from the enum: this one
    /// number is the entire difference between a merchant being paid and a
    /// merchant's money landing somewhere only a seed re-import reaches.
    /// The xpub depth check assumes one depth for every family; if a family's
    /// account path ever differs, its correct key would be refused.
    #[test]
    fn every_family_account_path_is_at_the_depth_the_xpub_check_expects() {
        for family in [ChainFamily::Evm, ChainFamily::Tron] {
            let derivations = family.account_path().split('/').skip(1).count();
            assert_eq!(derivations, usize::from(crate::wallet::ACCOUNT_XPUB_DEPTH));
        }
    }

    #[test]
    fn each_family_uses_its_own_slip44_coin_type() {
        assert_eq!(ChainFamily::Evm.coin_type(), 60);
        assert_eq!(ChainFamily::Tron.coin_type(), 195);
        assert_eq!(ChainFamily::Evm.account_path(), "m/44'/60'/0'");
        assert_eq!(ChainFamily::Tron.account_path(), "m/44'/195'/0'");
        assert_eq!(
            ChainFamily::Tron.derivation_path(3),
            "m/44'/195'/0'/0/3",
            "the path shown to a merchant has to carry the coin type, or it \
             says nothing they can check"
        );
    }

    /// An unknown namespace is refused, not quietly treated as Ethereum.
    #[test]
    fn an_unknown_namespace_has_no_family() {
        assert_eq!(ChainFamily::from_namespace("solana"), None);
        assert_eq!(ChainFamily::from_namespace("EIP155"), None);
        assert_eq!(ChainFamily::from_namespace(""), None);
        assert!(family_for_namespace("solana").is_err());
    }

    /// Tron's encoding against a published address.
    ///
    /// `TUEZSdKsoDHQMeZwihtdoBiN46zxhGWYdH` is what the standard BIP-39 test
    /// mnemonic derives at `m/44'/195'/0'/0/0`, whose key hashes to the bytes
    /// below. Checked against an independent implementation, not against this
    /// one.
    #[test]
    fn tron_addresses_are_base58check_over_the_0x41_prefix() {
        let bytes = Address::from_str("0xc8599111f29c1e1e061265b4af93ea1f274ad78a").unwrap();
        assert_eq!(
            ChainFamily::Tron.encode_address(bytes),
            "TUEZSdKsoDHQMeZwihtdoBiN46zxhGWYdH"
        );

        // The same bytes are a perfectly ordinary Ethereum address. Nothing
        // about them says which chain they belong on, which is the whole
        // difficulty.
        assert_eq!(
            ChainFamily::Evm.encode_address(bytes),
            "0xC8599111F29c1e1E061265b4AF93eA1F274aD78A"
        );
    }

    /// A leading zero byte survives base58's own leading-`1` rule.
    ///
    /// Not a curiosity: Tron's `0x41` prefix means the payload never starts
    /// with a zero, but the encoder is a general one and a version that
    /// dropped leading zeros would silently shorten some addresses. Encoding
    /// is checked by re-deriving the checksum rather than by eye.
    #[test]
    fn the_tron_checksum_covers_the_prefix_and_the_whole_address() {
        let addr = Address::from_str("0x0000000000000000000000000000000000000001").unwrap();
        let encoded = ChainFamily::Tron.encode_address(addr);
        let decoded = bs58::decode(&encoded).into_vec().unwrap();

        assert_eq!(decoded.len(), 25);
        assert_eq!(decoded[0], 0x41);
        assert_eq!(&decoded[1..21], addr.as_slice());

        use sha2::{Digest, Sha256};
        let expected = Sha256::digest(Sha256::digest(&decoded[..21]));
        assert_eq!(&decoded[21..], &expected[..4]);
        assert!(encoded.starts_with('T'));
    }
}
