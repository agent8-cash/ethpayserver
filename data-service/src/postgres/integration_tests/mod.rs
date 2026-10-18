//! Integration tests for PostgreSQL repository implementations.
//!
//! These tests require a running PostgreSQL instance with the schema applied.
//! Run with: `DATABASE_URL="postgres://..." cargo test -p data-service -- --ignored`
//!
//! Test modules:
//! - `invoice`: Invoice CRUD and query tests
//! - `payment`: Payment CRUD and confirmation tests
//! - `watched_address`: Watched address management tests
//! - `aggregation`: Multi-currency payment aggregation E2E tests
//! - `analytics`: Dashboard payment-volume aggregation
//! - `wallet`: Account wallet derivation and store resolution
//! - `wallet_migration`: the account-wallets migration run over old-shape data
//! - `backfill_truncated_payment_symbols`: the truncated-address backfill,
//!   resolving known tokens and falling back for the rest
//! - `bootstrap_admin`: the sole-account promotion, against an empty table,
//!   one account, two accounts, and a server that already has an admin
//! - `first_account_admin`: the first registered account becomes the server
//!   administrator, and the second does not
//! - `account_standing`: the version compare-and-set and the fail-open read
//! - `account_deletion`: what blocks deleting an account, against the real FKs
//! - `store_creation`: a store and the membership that owns it, as one unit
//! - `payout_claims`: which invoices a store's payouts already hold
//! - `email_change`: pending email changes leave the recovery salt untouched
//! - `webhook_delivery`: retries of one job collapse to one row, and a
//!   store's deliveries stay scoped to it
//! - `merchant_directory`: the plugin host's server-wide account/store list
//! - `installed_plugins`: what is installed survives a restart, and the
//!   audit trail outlives the plugin it describes
//! - `seeded_tokens`: the migration-seeded L2 and testnet tokens, resolved
//!   by `TokenReader::get_by_address` the way a real payment resolves them
//! - `expected_watch`: `expected_watched_addresses` excludes a still-active
//!   watch once its invoice resolves, not just once it is deactivated

mod account_deletion;
mod account_standing;
mod aggregation;
mod analytics;
mod backfill_truncated_payment_symbols;
mod bootstrap_admin;
mod chain_cursor;
mod email_change;
mod expected_watch;
mod first_account_admin;
mod installed_plugins;
mod invoice;
mod invoice_creation;
mod merchant_directory;
mod payment;
mod payout_claims;
mod seeded_tokens;
mod server_settings;
mod store_creation;
mod wallet;
mod wallet_migration;
mod wallet_namespace_migration;
mod watched_address;
mod webhook_delivery;

use chrono::Utc;
use types::{PaymentMethodId, PaymentOptionData, PaymentOptionId};

use super::tests::{
    assert_amount_eq, create_test_service, seeded_test_invoice, test_payment, unique_address,
};

// =============================================================================
// Shared test helpers
// =============================================================================

/// Create a basic payment option for testing.
pub(crate) fn test_payment_option(
    invoice_id: &types::InvoiceId,
    chain_id: &types::ChainId,
) -> PaymentOptionData {
    PaymentOptionData {
        id: PaymentOptionId::new(),
        invoice_id: invoice_id.clone(),
        payment_method_id: PaymentMethodId::new("ETH", chain_id),
        chain_id: chain_id.clone(),
        asset_symbol: "ETH".to_string(),
        token_address: None,
        decimals: 18,
        payment_address: format!("0x{:040x}", uuid::Uuid::new_v4().as_u128()),
        wallet_id: None,
        derivation_index: None,
        amount: "1000000000000000000".to_string(),
        rate: None,
        rate_at: None,
        is_active: true,
        created_at: Utc::now(),
    }
}

/// Create a payment option with specific asset, rate, and decimals.
pub(crate) fn test_payment_option_with_rate(
    invoice_id: &types::InvoiceId,
    chain_id: &types::ChainId,
    asset_symbol: &str,
    token_address: Option<String>,
    decimals: u8,
    amount: &str,
    rate: Option<String>,
) -> PaymentOptionData {
    PaymentOptionData {
        id: PaymentOptionId::new(),
        invoice_id: invoice_id.clone(),
        payment_method_id: PaymentMethodId::new(asset_symbol, chain_id),
        chain_id: chain_id.clone(),
        asset_symbol: asset_symbol.to_string(),
        token_address,
        decimals,
        payment_address: format!("0x{:040x}", uuid::Uuid::new_v4().as_u128()),
        wallet_id: None,
        derivation_index: None,
        amount: amount.to_string(),
        rate,
        rate_at: Some(Utc::now()),
        is_active: true,
        created_at: Utc::now(),
    }
}

/// Create a payment with credited_amount for aggregation testing.
pub(crate) fn test_payment_with_credit(
    invoice_id: &types::InvoiceId,
    payment_option_id: Option<uuid::Uuid>,
    chain_id: &types::ChainId,
    asset_symbol: &str,
    amount: &str,
    credited_amount: Option<String>,
    rate_used: Option<String>,
) -> types::traits::PaymentData {
    let has_credit = credited_amount.is_some();
    types::traits::PaymentData {
        id: uuid::Uuid::new_v4(),
        invoice_id: invoice_id.clone(),
        payment_option_id,
        chain_id: chain_id.clone(),
        asset_type: types::AssetType::Native,
        amount: amount.to_string(),
        asset_symbol: asset_symbol.to_string(),
        token_address: None,
        tx_hash: format!("0x{:064x}", uuid::Uuid::new_v4().as_u128()),
        block_number: Some(12345678),
        detected_at: Utc::now(),
        confirmed_at: None,
        from_address: Some("0xabcdef1234567890abcdef1234567890abcdef12".to_string()),
        reorged: false,
        extra: None,
        credited_amount,
        rate_used,
        rate_applied_at: if has_credit { Some(Utc::now()) } else { None },
    }
}
