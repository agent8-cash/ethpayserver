//! Offline tool to generate or derive the extended public key (xpub) a
//! merchant registers with `POST /wallets`.
//!
//! Links no HTTP client, so there is no code path here that could transmit a
//! mnemonic or a key anywhere. Run it disconnected if that isn't reassurance
//! enough on its own.
//!
//! ```text
//! derive-xpub generate       # new dedicated receiving wallet
//! derive-xpub from-existing  # derive from a mnemonic already held
//! ```
//!
//! Both take `--json`: stdout then carries one JSON object with `xpub`,
//! `path` and `verification_addresses`, and the generated mnemonic goes to
//! stderr instead, so it is still shown without polluting stdout.

use evm::{ChainFamily, HdWallet, VERIFICATION_ADDRESS_COUNT, generate_mnemonic};
use std::io::BufRead;
use std::process::ExitCode;

/// Machine-readable output for `--json`. Field names are a public contract.
#[derive(serde::Serialize)]
struct XpubExport {
    xpub: String,
    path: String,
    verification_addresses: Vec<String>,
}

/// Every payment this server derives is Ethereum/EVM; the tool has no reason
/// to ask which family, so it never does.
const FAMILY: ChainFamily = ChainFamily::Evm;

fn main() -> ExitCode {
    let json = std::env::args().skip(2).any(|arg| arg == "--json");
    match std::env::args().nth(1).as_deref() {
        Some("generate") => generate(json),
        Some("from-existing") => from_existing(json),
        _ => {
            print_usage();
            ExitCode::FAILURE
        }
    }
}

fn print_usage() {
    eprintln!(
        "usage: derive-xpub <generate|from-existing>\n\n\
         generate       creates a brand new mnemonic and prints its account xpub.\n\
         from-existing  reads a mnemonic from stdin and prints its account xpub.\n\n\
         Pass --json after either subcommand for machine-readable output.\n\n\
         Makes no network connections either way."
    );
}

fn generate(json: bool) -> ExitCode {
    let mnemonic = match generate_mnemonic(24) {
        Ok(m) => m,
        Err(e) => return fail(&format!("could not generate a mnemonic: {e}")),
    };
    if json {
        // Stdout is reserved for the JSON object; the mnemonic must still reach the user.
        eprintln!("mnemonic: {mnemonic}");
        return emit(&mnemonic, "", true);
    }
    println!(
        "New mnemonic - write it down somewhere offline and never type it into a \
         website:\n\nmnemonic: {mnemonic}\n"
    );
    println!(
        "This wallet exists to receive payments and nothing else. Don't reuse it \
         as a daily-driver wallet, and don't import it into a browser extension."
    );
    emit(&mnemonic, "", false)
}

fn from_existing(json: bool) -> ExitCode {
    eprintln!(
        "Paste the mnemonic on one line and press Enter. Do this only on a \
         machine you trust, offline if you can manage it - typing a seed phrase \
         into software is exactly the shape of a wallet-drain attack, and this \
         tool is not an exception to that."
    );
    let mut line = String::new();
    if std::io::stdin().lock().read_line(&mut line).is_err() {
        return fail("could not read from stdin");
    }
    let mnemonic = line.trim().to_string();
    if mnemonic.is_empty() {
        return fail("no mnemonic given");
    }

    eprintln!(
        "\nIf this wallet has a BIP-39 passphrase set (some wallets call it a \
         \"25th word\"), enter it now - leave blank if it doesn't have one. \
         Getting this wrong produces a different, wrong xpub with no error:"
    );
    let mut passphrase = String::new();
    if std::io::stdin().lock().read_line(&mut passphrase).is_err() {
        return fail("could not read from stdin");
    }
    // BIP-39 passphrases are used byte-for-byte, so `.trim()` (which strips
    // any leading/trailing whitespace, not just the newline `read_line`
    // leaves) would silently derive a different key for a passphrase that
    // legitimately starts or ends with a space - exactly the class of silent
    // mismatch the prompt above warns about. Only strip the line ending.
    let passphrase = passphrase.trim_end_matches(['\n', '\r']);

    emit(&mnemonic, passphrase, json)
}

fn emit(mnemonic: &str, passphrase: &str, json: bool) -> ExitCode {
    let wallet = match HdWallet::from_mnemonic(mnemonic, passphrase) {
        Ok(w) => w,
        Err(e) => return fail(&format!("invalid mnemonic: {e}")),
    };
    let xpub = match wallet.account_xpub_string_for(FAMILY) {
        Ok(x) => x,
        Err(e) => return fail(&format!("could not derive xpub: {e}")),
    };
    if json {
        return emit_json(&wallet, xpub);
    }
    println!("\naccount path: {}", FAMILY.account_path());
    println!("xpub:         {xpub}\n");
    println!(
        "First {VERIFICATION_ADDRESS_COUNT} receiving addresses - compare these against your \
         own wallet, and against `verification_addresses` from `POST /wallets`, \
         before a single invoice quotes one:\n"
    );
    print_check_addresses(&wallet)
}

fn emit_json(wallet: &HdWallet, xpub: String) -> ExitCode {
    let mut verification_addresses = Vec::with_capacity(VERIFICATION_ADDRESS_COUNT as usize);
    for index in 0..VERIFICATION_ADDRESS_COUNT {
        match wallet.derive_address_for(FAMILY, index) {
            Ok(address) => verification_addresses.push(FAMILY.encode_address(address)),
            Err(e) => return fail(&format!("could not derive address {index}: {e}")),
        }
    }
    let export = XpubExport {
        xpub,
        path: FAMILY.account_path(),
        verification_addresses,
    };
    match serde_json::to_string(&export) {
        Ok(json) => {
            println!("{json}");
            ExitCode::SUCCESS
        }
        Err(e) => fail(&format!("could not encode json: {e}")),
    }
}

fn print_check_addresses(wallet: &HdWallet) -> ExitCode {
    for index in 0..VERIFICATION_ADDRESS_COUNT {
        match wallet.derive_address_for(FAMILY, index) {
            Ok(address) => println!(
                "  [{index}] {} ({})",
                FAMILY.encode_address(address),
                FAMILY.derivation_path(index)
            ),
            Err(e) => return fail(&format!("could not derive address {index}: {e}")),
        }
    }
    ExitCode::SUCCESS
}

fn fail(message: &str) -> ExitCode {
    eprintln!("error: {message}");
    ExitCode::FAILURE
}
