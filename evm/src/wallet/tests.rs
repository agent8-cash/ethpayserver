use super::*;

// Standard test mnemonic (DO NOT USE IN PRODUCTION)
pub(super) const TEST_MNEMONIC: &str =
    "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

/// What a wallet exports for `TEST_MNEMONIC` at `m/44'/60'/0'` - the
/// string a merchant pastes to register an Ethereum receiving key.
pub(super) const EVM_ACCOUNT_XPUB: &str = "xpub6DCoCpSuQZB2jawqnGMEPS63ePKWkwWPH4TU45Q7LPXWuNd8TMtVxRrgjtEshuqpK3mdhaWHPFsBngh5GFZaM6si3yZdUsT8ddYM3PwnATt";

/// The same seed at `m/44'/195'/0'` - Tron. Note that nothing about the
/// two strings says which is which.
pub(super) const TRON_ACCOUNT_XPUB: &str = "xpub6D1AabNHCupeiLM65ZR9UStMhJ1vCpyV4XbZdyhMZBiJXALQtmn9p42VTQckoHVn8WNqS7dqnJokZHAHcHGoaQgmv8D45oNUKx6DZMNZBCd";

#[test]
fn test_wallet_from_mnemonic() {
    let wallet = HdWallet::from_mnemonic(TEST_MNEMONIC, "").unwrap();
    let address = wallet.derive_address(0).unwrap();

    // Known address for this mnemonic at m/44'/60'/0'/0/0
    // Compare lowercase since Address debug format doesn't preserve checksum
    assert_eq!(
        format!("{:?}", address).to_lowercase(),
        "0x9858effd232b4033e47d90003d41ec34ecaeda94"
    );
}

#[test]
fn test_derive_multiple_addresses() {
    let wallet = HdWallet::from_mnemonic(TEST_MNEMONIC, "").unwrap();
    let addresses = wallet.derive_addresses(0, 5).unwrap();

    assert_eq!(addresses.len(), 5);

    // Each address should be unique
    let mut unique = addresses.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), 5);
}

#[test]
fn test_deterministic_derivation() {
    let wallet1 = HdWallet::from_mnemonic(TEST_MNEMONIC, "").unwrap();
    let wallet2 = HdWallet::from_mnemonic(TEST_MNEMONIC, "").unwrap();

    for i in 0..10 {
        assert_eq!(
            wallet1.derive_address(i).unwrap(),
            wallet2.derive_address(i).unwrap()
        );
    }
}

#[test]
fn test_passphrase_changes_addresses() {
    let wallet1 = HdWallet::from_mnemonic(TEST_MNEMONIC, "").unwrap();
    let wallet2 = HdWallet::from_mnemonic(TEST_MNEMONIC, "secret").unwrap();

    assert_ne!(
        wallet1.derive_address(0).unwrap(),
        wallet2.derive_address(0).unwrap()
    );
}

#[test]
fn test_invalid_mnemonic() {
    let result = HdWallet::from_mnemonic("invalid mnemonic phrase", "");
    assert!(result.is_err());
}

#[test]
fn test_generate_mnemonic() {
    let mnemonic = generate_mnemonic(12).unwrap();
    let words: Vec<&str> = mnemonic.split_whitespace().collect();
    assert_eq!(words.len(), 12);
    assert!(validate_mnemonic(&mnemonic));

    let mnemonic = generate_mnemonic(24).unwrap();
    let words: Vec<&str> = mnemonic.split_whitespace().collect();
    assert_eq!(words.len(), 24);
    assert!(validate_mnemonic(&mnemonic));
}

#[test]
fn test_validate_mnemonic() {
    assert!(validate_mnemonic(TEST_MNEMONIC));
    assert!(!validate_mnemonic("invalid mnemonic"));
    assert!(!validate_mnemonic(""));
}

#[test]
fn test_custom_derivation_path() {
    let wallet = HdWallet::from_mnemonic(TEST_MNEMONIC, "").unwrap();

    // Derive at a custom path
    let addr1 = wallet.derive_address_at_path("m/44'/60'/0'/0/0").unwrap();
    let addr2 = wallet.derive_address(0).unwrap();

    // Should be the same
    assert_eq!(addr1, addr2);
}

#[test]
fn test_xpub_deriver_matches_wallet() {
    let wallet = HdWallet::from_mnemonic(TEST_MNEMONIC, "").unwrap();
    let xpub = wallet.account_xpub().unwrap();
    let xpub_str = MainnetEncoder::xpub_to_base58(&xpub).unwrap();
    let deriver = XpubDeriver::from_xpub("eip155", &xpub_str).unwrap();

    // XpubDeriver should derive the same addresses as HdWallet
    for i in 0..5 {
        assert_eq!(
            wallet.derive_address(i).unwrap(),
            deriver.derive_evm_address(i).unwrap()
        );
    }
}

/// The coin type is honoured, not merely the encoding swapped.
///
/// This is the assertion the whole family-scoping change rests on, and the
/// one that a plausible-looking wrong implementation fails. Tron addresses
/// are base58check over the same 20 bytes Ethereum renders as hex, so it
/// is entirely possible to "add Tron support" by re-encoding an Ethereum
/// key and produce valid, checksum-correct `T…` addresses for every index.
/// They would be addresses at `m/44'/60'`, and the merchant's Tron wallet
/// looks under `m/44'/195'` - so the money arrives somewhere only a seed
/// re-import at a non-standard path can reach.
///
/// So: from one seed, the Tron address must not be the Tron encoding of
/// the Ethereum address. It must be a different key entirely.
#[test]
fn a_tron_address_is_not_the_evm_address_in_another_alphabet() {
    use crate::family::ChainFamily;

    let wallet = HdWallet::from_mnemonic(TEST_MNEMONIC, "").unwrap();

    for index in 0..3u32 {
        let evm_bytes = wallet.derive_address_for(ChainFamily::Evm, index).unwrap();
        let tron_bytes = wallet.derive_address_for(ChainFamily::Tron, index).unwrap();

        assert_ne!(
            evm_bytes, tron_bytes,
            "index {index}: m/44'/60'/0'/0/{index} and m/44'/195'/0'/0/{index}                  produced the same key - the coin type is being ignored"
        );

        // And stated the way the bug would actually appear: the Tron
        // address is not the EVM address wearing base58.
        assert_ne!(
            ChainFamily::Tron.encode_address(tron_bytes),
            ChainFamily::Tron.encode_address(evm_bytes),
            "index {index}: the Tron address is just the EVM key re-encoded"
        );
    }
}

/// Both families' addresses for one seed, against an independent
/// derivation of the standard BIP-39 test mnemonic.
///
/// Published values, not values this code produced: an implementation that
/// is self-consistently wrong about the coin type passes every relative
/// assertion in this file, and only a fixed external vector catches it.
/// The Ethereum value is the one this module has always asserted; the Tron
/// values are at `m/44'/195'/0'/0/{0,1,2}`.
#[test]
fn one_seed_derives_the_published_addresses_for_each_family() {
    use crate::family::ChainFamily;

    let wallet = HdWallet::from_mnemonic(TEST_MNEMONIC, "").unwrap();

    let evm: Vec<String> = (0..3)
        .map(|i| {
            ChainFamily::Evm.encode_address(wallet.derive_address_for(ChainFamily::Evm, i).unwrap())
        })
        .collect();
    assert_eq!(
        evm[0].to_lowercase(),
        "0x9858effd232b4033e47d90003d41ec34ecaeda94"
    );

    let tron: Vec<String> = (0..3)
        .map(|i| {
            ChainFamily::Tron
                .encode_address(wallet.derive_address_for(ChainFamily::Tron, i).unwrap())
        })
        .collect();
    assert_eq!(
        tron,
        vec![
            "TUEZSdKsoDHQMeZwihtdoBiN46zxhGWYdH".to_string(),
            "TSeJkUh4Qv67VNFwY8LaAxERygNdy6NQZK".to_string(),
            "TYJPRrdB5APNeRs4R7fYZSwW3TcrTKw2gx".to_string(),
        ]
    );
}

/// The two families' account xpubs are indistinguishable by inspection.
///
/// The premise of the whole design. If a server could tell them apart it
/// would not need to ask, and every merchant-facing warning here would be
/// unnecessary ceremony. It cannot: same version bytes, same alphabet,
/// same length, both accepted by the only check a merchant's paste is
/// subjected to.
///
/// These are the strings a merchant actually pastes - what a wallet
/// exports for the standard BIP-39 test mnemonic at `m/44'/60'/0'` and
/// `m/44'/195'/0'` - rather than what this crate's encoder emits, which is
/// a `zpub` and so would hide the very similarity being asserted.
#[test]
fn an_ethereum_xpub_and_a_tron_xpub_cannot_be_told_apart() {
    assert_ne!(
        EVM_ACCOUNT_XPUB, TRON_ACCOUNT_XPUB,
        "different keys, or the coin type did nothing"
    );
    assert_eq!(EVM_ACCOUNT_XPUB.len(), TRON_ACCOUNT_XPUB.len());
    assert!(EVM_ACCOUNT_XPUB.starts_with("xpub") && TRON_ACCOUNT_XPUB.starts_with("xpub"));
    assert!(
        validate_xpub(EVM_ACCOUNT_XPUB) && validate_xpub(TRON_ACCOUNT_XPUB),
        "both pass the only check a merchant's paste is subjected to"
    );
}

/// Registering a key by the string a merchant pastes produces the
/// published addresses for its family - and, for the same seed, the two
/// families produce different ones.
///
/// This is the entry point that matters: `POST /wallets` hands an xpub
/// string and a namespace to exactly this pair of calls. The Ethereum
/// value is the one this module has asserted since it was written; the
/// Tron values come from an independent derivation at `m/44'/195'/0'/0/i`.
#[test]
fn a_registered_xpub_derives_its_own_family_published_addresses() {
    let evm = XpubDeriver::from_xpub("eip155", EVM_ACCOUNT_XPUB).unwrap();
    assert_eq!(
        evm.derive_address(0).unwrap().to_lowercase(),
        "0x9858effd232b4033e47d90003d41ec34ecaeda94"
    );

    let tron = XpubDeriver::from_xpub("tron", TRON_ACCOUNT_XPUB).unwrap();
    let first_three: Vec<String> = (0..3).map(|i| tron.derive_address(i).unwrap()).collect();
    assert_eq!(
        first_three,
        vec![
            "TUEZSdKsoDHQMeZwihtdoBiN46zxhGWYdH".to_string(),
            "TSeJkUh4Qv67VNFwY8LaAxERygNdy6NQZK".to_string(),
            "TYJPRrdB5APNeRs4R7fYZSwW3TcrTKw2gx".to_string(),
        ]
    );

    // The Ethereum key run through Tron's encoding is a valid `T…`
    // address, and it is not any of those. That is the failure mode: it
    // looks right, it passes every checksum, and the merchant's Tron
    // wallet never shows it.
    let misfiled = XpubDeriver::from_xpub("tron", EVM_ACCOUNT_XPUB).unwrap();
    let wrong = misfiled.derive_address(0).unwrap();
    assert!(wrong.starts_with('T'));
    assert!(!first_three.contains(&wrong));
}

/// A Tron key refuses to be used where EVM address bytes are expected.
///
/// `derive_evm_address` feeds the chain monitor and the watched-address
/// table, which mean an address on an EVM chain. Bytes from a Tron key put
/// there would have the server watching an Ethereum address nobody will
/// ever pay.
#[test]
fn a_tron_key_cannot_be_used_as_an_evm_key() {
    use crate::family::ChainFamily;

    let wallet = HdWallet::from_mnemonic(TEST_MNEMONIC, "").unwrap();
    let xpub = MainnetEncoder::xpub_to_base58(&wallet.account_xpub_for(ChainFamily::Tron).unwrap())
        .unwrap();

    let deriver = XpubDeriver::from_xpub("tron", &xpub).unwrap();
    assert!(deriver.derive_evm_address(0).is_err());
    assert!(deriver.derive_address(0).unwrap().starts_with('T'));
    assert_eq!(deriver.derivation_path(0), "m/44'/195'/0'/0/0");

    // And a namespace with no derivation at all is refused outright,
    // rather than quietly treated as Ethereum.
    assert!(XpubDeriver::from_xpub("solana", &xpub).is_err());
}

#[test]
fn test_derive_private_key() {
    let wallet = HdWallet::from_mnemonic(TEST_MNEMONIC, "").unwrap();
    let key = wallet.derive_private_key(0).unwrap();

    // Should be 32 bytes
    assert_eq!(key.len(), 32);

    // Should be deterministic
    let key2 = wallet.derive_private_key(0).unwrap();
    assert_eq!(key, key2);

    // Different indexes should produce different keys
    let key3 = wallet.derive_private_key(1).unwrap();
    assert_ne!(key, key3);
}

#[test]
fn test_xpub_validation() {
    let wallet = HdWallet::from_mnemonic(TEST_MNEMONIC, "").unwrap();
    let xpub = wallet.account_xpub().unwrap();
    let xpub_str = MainnetEncoder::xpub_to_base58(&xpub).unwrap();

    assert!(validate_xpub(&xpub_str));
    assert!(!validate_xpub("invalid-xpub"));
    assert!(!validate_xpub(""));
}

/// A private key must never be accepted where a public one is asked for.
///
/// The whole custody story rests on this: a merchant hands over an xpub, the
/// server derives receive addresses from it, and nothing here can move their
/// funds because nothing here can sign. An `xprv` pasted into the same
/// field, by a merchant who does not know the difference or who copied the
/// wrong line out of their wallet, would put a spending key in our database
/// and silently make us custodial.
///
/// `xpub_from_base58` rejects it on the version-byte prefix (0x0488ADE4 for
/// private, 0x0488B21E for public). That is a property of the decoder rather
/// than a check anyone wrote here, which is exactly why it deserves a test:
/// nothing in this file would notice if it stopped being true.
#[test]
fn an_xprv_is_never_accepted_as_an_xpub() {
    // BIP-32 test vector 1 master keys - a real pair, same seed.
    const XPRV: &str = "xprv9s21ZrQH143K3QTDL4LXw2F7HEK3wJUD2nW2nRk4stbPy6cq3jPPqjiChkVvvNKmPGJxWUtg6LnF5kejMRNNU3TGtRBeJgk33yuGBxrMPHi";
    const XPUB: &str = "xpub661MyMwAqRbcFtXgS5sYJABqqG9YLmC4Q1Rdap9gSE8NqtwybGhePY2gZ29ESFjqJoCu1Rupje8YtGqsefD265TMg7usUDFdp6W1EGMcet8";

    assert!(validate_xpub(XPUB), "a valid xpub must be accepted");
    assert!(
        !validate_xpub(XPRV),
        "an xprv was accepted as an xpub: the server would be holding a spending key"
    );
}

#[test]
fn looks_like_a_private_key_flags_mainnet_and_testnet_prefixes() {
    assert!(looks_like_a_private_key(
        "xprv9s21ZrQH143K3QTDL4LXw2F7HEK3wJUD2nW2nRk4stbPy6cq3jPPqjiChkVvvNKmPGJxWUtg6LnF5kejMRNNU3TGtRBeJgk33yuGBxrMPHi"
    ));
    assert!(looks_like_a_private_key("  tprv8ZgxMBicQKsPd...  "));
    assert!(!looks_like_a_private_key(
        "xpub661MyMwAqRbcFtXgS5sYJABqqG9YLmC4Q1Rdap9gSE8NqtwybGhePY2gZ29ESFjqJoCu1Rupje8YtGqsefD265TMg7usUDFdp6W1EGMcet8"
    ));
    assert!(!looks_like_a_private_key("not-a-key-at-all"));
    assert!(!looks_like_a_private_key(""));
}

#[test]
fn looks_like_a_private_key_flags_raw_hex_secp256k1_keys() {
    // The shape MetaMask and similar wallets export a raw private key in:
    // 32 bytes as hex, with or without the `0x` prefix.
    const HEX_64: &str = "5176b09f63694525911cea338ef089c6c304288e3c769bbb4e99d23d1a1e4f68";

    assert!(looks_like_a_private_key(HEX_64));
    assert!(looks_like_a_private_key(&format!("0x{HEX_64}")));
    assert!(looks_like_a_private_key(&format!("  0x{HEX_64}  ")));
    // One character short or long is no longer a plausible 32-byte key.
    assert!(!looks_like_a_private_key(&HEX_64[1..]));
    assert!(!looks_like_a_private_key(&format!("{HEX_64}a")));
    // Right length, but not hex - `g` is outside the hex alphabet.
    assert!(!looks_like_a_private_key(&"g".repeat(64)));
}

fn xpub_string_at(path: &str) -> String {
    let wallet = HdWallet::from_mnemonic(TEST_MNEMONIC, "").unwrap();
    let key = wallet
        .master_key
        .derive_path(DerivationPath::from_str(path).unwrap())
        .unwrap();
    MainnetEncoder::xpub_to_base58(&key.verify_key()).unwrap()
}

/// The receive-branch key (`m/44'/60'/0'/0`) is the easy wrong paste: it
/// parses, and appending `0/i` to it produces addresses the merchant's
/// wallet never shows. It is refused with the depth named, while the real
/// account key - the string a wallet exports, with the published first
/// address - and a dedicated hardened account both still pass.
#[test]
fn a_child_depth_xpub_is_refused_naming_its_depth() {
    let receive_branch = xpub_string_at("m/44'/60'/0'/0");
    assert!(
        validate_xpub(&receive_branch),
        "it parses, which is the trap"
    );
    let problem = xpub_depth_problem(&receive_branch).expect("depth 4 must be refused");
    assert!(problem.contains("depth 4"), "message was: {problem}");

    // What accepting it would have done: not the published address.
    let misderived = XpubDeriver::from_xpub("eip155", &receive_branch).unwrap();
    assert_ne!(
        misderived.derive_address(0).unwrap().to_lowercase(),
        "0x9858effd232b4033e47d90003d41ec34ecaeda94"
    );

    let master = xpub_string_at("m");
    assert!(xpub_depth_problem(&master).unwrap().contains("depth 0"));

    // Control: the exported account key and a hardened account are depth 3.
    assert_eq!(xpub_depth_problem(EVM_ACCOUNT_XPUB), None);
    assert_eq!(xpub_depth_problem(TRON_ACCOUNT_XPUB), None);
    assert_eq!(xpub_depth_problem(&xpub_string_at("m/44'/60'/7'")), None);
    assert_eq!(xpub_depth_problem("not an xpub"), None);

    // Existing derivation is untouched.
    let evm = XpubDeriver::from_xpub("eip155", EVM_ACCOUNT_XPUB).unwrap();
    assert_eq!(
        evm.derive_address(0).unwrap().to_lowercase(),
        "0x9858effd232b4033e47d90003d41ec34ecaeda94"
    );
}
