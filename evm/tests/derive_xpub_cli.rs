//! Exercises the `derive-xpub` binary itself, not just the library functions
//! it calls.
//!
//! Every unit test for the underlying derivation lives in `evm::wallet` and
//! calls `HdWallet` directly - none of them run through the binary's argv
//! dispatch, its two sequential stdin reads (mnemonic, then passphrase) for
//! `from-existing`, or its printed output. That CLI glue is the literal
//! entry point the onboarding docs tell every merchant to run, so a bug in
//! argument matching, stdin read order, or which field gets printed would
//! reach a merchant with zero test coverage catching it first.

use evm::{ChainFamily, HdWallet};
use std::io::Write;
use std::process::{Command, Stdio};

const TEST_MNEMONIC: &str =
    "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

/// Matches `EVM_ACCOUNT_XPUB` in `evm/src/wallet.rs`'s test module - kept as
/// an independent literal rather than importing the library constant, so
/// this test still catches a regression that broke the constant itself.
const EVM_ACCOUNT_XPUB: &str = "xpub6DCoCpSuQZB2jawqnGMEPS63ePKWkwWPH4TU45Q7LPXWuNd8TMtVxRrgjtEshuqpK3mdhaWHPFsBngh5GFZaM6si3yZdUsT8ddYM3PwnATt";

/// Matches `EVM_ACCOUNT_XPUB_WITH_PASSPHRASE` in `evm/src/wallet.rs`.
const EVM_ACCOUNT_XPUB_WITH_PASSPHRASE: &str = "xpub6Bmqz11Kt5qtj3xbXZkzEyYw43EDFGCon5GzC4udf7DPugyKjVppdX2amQZrGs4rqAJH79pDtge2UDENZzjz9DgcV3WmfbwYAXj2epC5cgz";

fn run_from_existing(mnemonic: &str, passphrase: &str) -> String {
    let mut child = Command::new(env!("CARGO_BIN_EXE_derive-xpub"))
        .arg("from-existing")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn derive-xpub");

    let mut stdin = child.stdin.take().expect("stdin piped");
    write!(stdin, "{mnemonic}\n{passphrase}\n").expect("write to stdin");
    drop(stdin);

    let output = child.wait_with_output().expect("wait for derive-xpub");
    assert!(
        output.status.success(),
        "derive-xpub exited non-zero: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("utf8 stdout")
}

/// The path a merchant pasting an existing seed phrase with no passphrase
/// actually walks: two lines on stdin (mnemonic, then a blank line), and the
/// xpub a real wallet would show must appear in the printed output.
#[test]
fn from_existing_with_no_passphrase_prints_the_expected_xpub() {
    let stdout = run_from_existing(TEST_MNEMONIC, "");
    assert!(
        stdout.contains(EVM_ACCOUNT_XPUB),
        "expected {EVM_ACCOUNT_XPUB} in output, got:\n{stdout}"
    );
}

/// The second stdin read is the passphrase line, not discarded or merged
/// into the first - if it were, this would print `EVM_ACCOUNT_XPUB` (the
/// empty-passphrase key) instead of the passphrase-protected one.
#[test]
fn from_existing_with_a_passphrase_prints_the_passphrase_protected_xpub() {
    let stdout = run_from_existing(TEST_MNEMONIC, "secret");
    assert!(
        stdout.contains(EVM_ACCOUNT_XPUB_WITH_PASSPHRASE),
        "expected {EVM_ACCOUNT_XPUB_WITH_PASSPHRASE} in output, got:\n{stdout}"
    );
    assert!(
        !stdout.contains(EVM_ACCOUNT_XPUB),
        "output contains the empty-passphrase xpub - the passphrase line was \
         dropped or ignored:\n{stdout}"
    );
}

/// A BIP-39 passphrase is used byte-for-byte, so leading/trailing spaces a
/// merchant genuinely typed must survive the two stdin reads intact - only
/// the trailing line ending `read_line` leaves behind may be stripped. If
/// this ever regressed to a general `.trim()`, this would silently start
/// deriving a different, wrong xpub with no error, and every other test in
/// this file (which use whitespace-free passphrases) would keep passing.
#[test]
fn from_existing_preserves_leading_and_trailing_passphrase_whitespace() {
    let padded_passphrase = "  secret  ";
    let expected_xpub = HdWallet::from_mnemonic(TEST_MNEMONIC, padded_passphrase)
        .expect("valid mnemonic")
        .account_xpub_string_for(ChainFamily::Evm)
        .expect("derive xpub");

    let stdout = run_from_existing(TEST_MNEMONIC, padded_passphrase);
    assert!(
        stdout.contains(&expected_xpub),
        "expected the whitespace-preserving xpub {expected_xpub} in output, got:\n{stdout}"
    );
    assert!(
        !stdout.contains(EVM_ACCOUNT_XPUB_WITH_PASSPHRASE),
        "output matches the trimmed-passphrase xpub - leading/trailing whitespace was \
         stripped instead of preserved:\n{stdout}"
    );
}

/// `generate` takes no stdin and must still succeed, print an
/// `xpub`-prefixed key, and - the part format-only checks would miss - that
/// key must actually derive from the mnemonic printed in the same
/// invocation, not just look like a plausible xpub next to it.
///
/// The "expected" value below is computed with the same
/// `account_xpub_string_for` call the binary itself makes, because `generate`
/// mints a fresh random mnemonic every run and there is no fixed vector to
/// compare against. That call's own correctness - the derivation path, not
/// just the CLI's plumbing - is pinned independently in
/// `evm/src/wallet/xpub_encoding_tests.rs`'s
/// `account_xpub_string_matches_what_a_real_wallet_exports`, which checks it
/// against a published address for a fixed test mnemonic. This test's job is
/// narrower: prove the binary prints the key it actually derived, not
/// re-prove that the derivation itself is correct.
#[test]
fn generate_prints_an_xpub_matching_its_own_mnemonic() {
    let output = Command::new(env!("CARGO_BIN_EXE_derive-xpub"))
        .arg("generate")
        .output()
        .expect("run derive-xpub generate");
    assert!(
        output.status.success(),
        "derive-xpub generate exited non-zero: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");

    let mnemonic = stdout
        .lines()
        .find(|line| line.trim_start().starts_with("mnemonic:"))
        .map(|line| {
            line.trim_start()
                .strip_prefix("mnemonic:")
                .expect("prefix checked above")
                .trim()
        })
        .filter(|line| !line.is_empty())
        .expect("mnemonic line present");

    let xpub_line = stdout
        .lines()
        .find(|line| line.trim_start().starts_with("xpub:"))
        .expect("xpub line present");
    let key = xpub_line
        .trim_start()
        .strip_prefix("xpub:")
        .expect("prefix checked above")
        .trim();
    assert!(
        key.starts_with("xpub"),
        "printed key doesn't look like an xpub-prefixed key: {xpub_line}"
    );

    let expected_xpub = HdWallet::from_mnemonic(mnemonic, "")
        .expect("generated mnemonic must be valid")
        .account_xpub_string_for(ChainFamily::Evm)
        .expect("derive xpub");
    assert_eq!(
        key, expected_xpub,
        "printed xpub does not derive from the printed mnemonic \
         (mnemonic: {mnemonic})"
    );
}

/// A merchant hitting Enter on an empty line (or piping an empty file) must
/// get a clean, loud failure - not a panic and not a plausible-looking xpub
/// derived from an empty string.
#[test]
fn from_existing_with_a_blank_mnemonic_fails_cleanly() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_derive-xpub"))
        .arg("from-existing")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn derive-xpub");

    let mut stdin = child.stdin.take().expect("stdin piped");
    writeln!(stdin).expect("write to stdin");
    drop(stdin);

    let output = child.wait_with_output().expect("wait for derive-xpub");
    assert!(
        !output.status.success(),
        "derive-xpub should reject a blank mnemonic, got:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8(output.stderr).expect("utf8 stderr");
    assert!(
        stderr.contains("no mnemonic given"),
        "expected the blank-mnemonic error, got:\n{stderr}"
    );
}

/// A typo'd or garbled seed phrase must fail loud, not silently derive a
/// plausible-looking but wrong xpub the merchant would register in good
/// faith - the same class of failure this tool exists to prevent.
#[test]
fn from_existing_with_an_invalid_mnemonic_fails_cleanly() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_derive-xpub"))
        .arg("from-existing")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn derive-xpub");

    let mut stdin = child.stdin.take().expect("stdin piped");
    write!(stdin, "not a valid bip39 mnemonic at all\n\n").expect("write to stdin");
    drop(stdin);

    let output = child.wait_with_output().expect("wait for derive-xpub");
    assert!(
        !output.status.success(),
        "derive-xpub should reject an invalid mnemonic, got:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8(output.stderr).expect("utf8 stderr");
    assert!(
        stderr.contains("invalid mnemonic"),
        "expected the invalid-mnemonic error, got:\n{stderr}"
    );
}

/// Counts the `  [n] address (path)` lines in human-mode output.
fn count_check_addresses(stdout: &str) -> usize {
    stdout
        .lines()
        .filter(|line| line.trim_start().starts_with('['))
        .count()
}

/// Parses a `--json` stdout and asserts it is exactly one object with the
/// three documented keys and nothing else.
fn parse_json_export(stdout: &str) -> serde_json::Map<String, serde_json::Value> {
    let value: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("stdout must be one JSON document");
    let object = value
        .as_object()
        .expect("JSON output must be an object")
        .clone();
    let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, ["path", "verification_addresses", "xpub"]);
    object
}

/// `generate --json` must put exactly one JSON object on stdout, its xpub
/// must derive from the mnemonic (which travels on stderr so it is not lost),
/// and its address count must match what the human output prints.
#[test]
fn generate_json_emits_the_export_object_and_matches_human_address_count() {
    let json_out = Command::new(env!("CARGO_BIN_EXE_derive-xpub"))
        .args(["generate", "--json"])
        .output()
        .expect("run derive-xpub generate --json");
    assert!(
        json_out.status.success(),
        "generate --json exited non-zero: {}",
        String::from_utf8_lossy(&json_out.stderr)
    );
    let stdout = String::from_utf8(json_out.stdout).expect("utf8 stdout");
    let export = parse_json_export(&stdout);

    let xpub = export["xpub"].as_str().expect("xpub is a string");
    assert!(xpub.starts_with("xpub"), "xpub not xpub-prefixed: {xpub}");
    assert_eq!(
        export["path"].as_str(),
        Some(ChainFamily::Evm.account_path().as_str())
    );
    let addresses = export["verification_addresses"]
        .as_array()
        .expect("verification_addresses is an array");
    assert!(addresses.iter().all(serde_json::Value::is_string));

    let stderr = String::from_utf8(json_out.stderr).expect("utf8 stderr");
    let mnemonic = stderr
        .lines()
        .find_map(|line| line.trim_start().strip_prefix("mnemonic:"))
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .expect("mnemonic printed on stderr in --json mode");
    let expected_xpub = HdWallet::from_mnemonic(mnemonic, "")
        .expect("generated mnemonic must be valid")
        .account_xpub_string_for(ChainFamily::Evm)
        .expect("derive xpub");
    assert_eq!(
        xpub, expected_xpub,
        "json xpub does not derive from the mnemonic"
    );

    let human = Command::new(env!("CARGO_BIN_EXE_derive-xpub"))
        .arg("generate")
        .output()
        .expect("run derive-xpub generate");
    let human_stdout = String::from_utf8(human.stdout).expect("utf8 stdout");
    assert_eq!(
        addresses.len(),
        count_check_addresses(&human_stdout),
        "verification_addresses count differs from human output"
    );
}

/// `from-existing --json` on a known mnemonic must yield the same xpub and the
/// same addresses as the human output for that mnemonic.
#[test]
fn from_existing_json_matches_the_human_output() {
    let human = run_from_existing(TEST_MNEMONIC, "");

    let mut child = Command::new(env!("CARGO_BIN_EXE_derive-xpub"))
        .args(["from-existing", "--json"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn derive-xpub");
    let mut stdin = child.stdin.take().expect("stdin piped");
    write!(stdin, "{TEST_MNEMONIC}\n\n").expect("write to stdin");
    drop(stdin);
    let output = child.wait_with_output().expect("wait for derive-xpub");
    assert!(
        output.status.success(),
        "from-existing --json exited non-zero"
    );
    let export = parse_json_export(&String::from_utf8(output.stdout).expect("utf8"));

    assert_eq!(export["xpub"].as_str(), Some(EVM_ACCOUNT_XPUB));
    assert_eq!(
        export["path"].as_str(),
        Some(ChainFamily::Evm.account_path().as_str())
    );
    let addresses = export["verification_addresses"].as_array().expect("array");
    assert_eq!(addresses.len(), count_check_addresses(&human));
    for address in addresses {
        let address = address.as_str().expect("string");
        assert!(
            human.contains(address),
            "address {address} missing from human output"
        );
    }
}

/// An xprv is not a mnemonic. `--json` must refuse it like the human path
/// does: nonzero exit, an error on stderr, and no JSON on stdout.
#[test]
fn from_existing_json_refuses_an_xprv() {
    let xprv = "xprv9s21ZrQH143K3QTDL4LXw2F7HEK3wJUD2nW2nRk4stbPy6cq3jPPqjiChkVvvNKmPGJxWUtg6LnF5kejMRNNU3TGtRBeJgk33yuGBxrMPHi";
    let mut child = Command::new(env!("CARGO_BIN_EXE_derive-xpub"))
        .args(["from-existing", "--json"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn derive-xpub");
    let mut stdin = child.stdin.take().expect("stdin piped");
    write!(stdin, "{xprv}\n\n").expect("write to stdin");
    drop(stdin);
    let output = child.wait_with_output().expect("wait for derive-xpub");

    assert!(!output.status.success(), "xprv must be refused with --json");
    assert!(
        output.stdout.is_empty(),
        "no stdout on failure: {:?}",
        output.stdout
    );
    let stderr = String::from_utf8(output.stderr).expect("utf8 stderr");
    assert!(
        stderr.contains("error:"),
        "expected an error on stderr, got:\n{stderr}"
    );
}
