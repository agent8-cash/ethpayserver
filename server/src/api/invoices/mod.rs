//! Invoice management API endpoints.
//!
//! All endpoints require authentication. Users can only access invoices
//! for stores they are members of.

mod cancel;
mod crud;
mod csv_export;
mod ledger_export;
mod list;
mod lookup;
mod payment_options;
mod payments;
mod scope;
mod types;

#[cfg(test)]
mod tests;

pub use cancel::*;
pub use crud::*;
pub use csv_export::*;
pub use ledger_export::*;
pub use list::*;
pub use lookup::*;
pub use payments::*;
pub use types::*;

pub(crate) use scope::{StoreScope, narrow_scope_by_key, verify_store_access_for_query};

// Re-export pub(crate) items from sub-modules for tests.
#[cfg(test)]
pub(crate) use crud::store_has_no_wallet;
#[cfg(test)]
pub(crate) use csv_export::{csv_escape_field, csv_row};
#[cfg(test)]
pub(crate) use lookup::is_valid_tx_hash;

// Reused by the plugin host API's own-store invoice creation, so a
// plugin-issued invoice derives its address exactly the way this endpoint
// does rather than through a second implementation that could drift from it.
pub(crate) use payment_options::derive_payment_options;

use axum::{Json, http::StatusCode};

use ::types::{InvoiceId, InvoiceReader, traits::InvoiceData};
use auth::{SessionService, repository::UserStoreRepository};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;

use crate::api::extractors::key_grants_store_permission;
use crate::services::EVMMonitor;
use crate::state::PgAppState;

/// `ethpay.store.canviewinvoices` - the permission that gates every read of
/// invoice or payment data below. A key's stored scope must grant this on a
/// store, on top of whatever the owner's own membership already allows, or a
/// key narrowed to (say) `cancreateinvoice` alone would still be able to read
/// every invoice and payment the owner can see - the same "key exceeds its
/// declared scope" gap this ticket exists to close, just on the read side.
pub(crate) const VIEW_INVOICES: &str = "ethpay.store.canviewinvoices";

/// Build a JSON error response for the create-invoice endpoint.
pub(crate) fn invoice_error(
    status: StatusCode,
    code: &str,
    message: &str,
) -> (StatusCode, Json<serde_json::Value>) {
    (
        status,
        Json(serde_json::json!({"error": code, "message": message})),
    )
}

/// Maximum age (seconds) before an exchange rate is rejected outright.
/// Override with `RATE_STALE_REJECT_SECS` env var. Default: 300 (5 minutes).
pub(crate) fn rate_stale_reject_secs() -> i64 {
    std::env::var("RATE_STALE_REJECT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(300)
}

/// Age (seconds) at which an exchange rate triggers a staleness warning.
/// Override with `RATE_STALE_WARN_SECS` env var. Default: 60.
pub(crate) fn rate_stale_warn_secs() -> i64 {
    std::env::var("RATE_STALE_WARN_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(60)
}

/// Convert an amount to crypto smallest units using the exchange rate.
///
/// # Arguments
/// * `amount` - Amount in invoice currency (e.g., "100.00" USD or "0.5" BTC)
/// * `rate` - Exchange rate: 1 invoice_currency = rate crypto_units
/// * `decimals` - Number of decimals for the crypto asset (e.g., 18 for ETH)
///
/// # Returns
/// Amount in smallest crypto units as a string (e.g., wei for ETH).
pub(crate) fn convert_to_crypto_smallest_unit(
    amount: &str,
    rate: Decimal,
    decimals: u8,
) -> Result<String, &'static str> {
    // Parse amount
    let parsed_amount: Decimal = amount.parse().map_err(|_| "Invalid amount")?;

    // Validate amount is positive
    if parsed_amount <= Decimal::ZERO {
        return Err("Amount must be positive");
    }

    // Calculate crypto amount: amount * rate
    let crypto_amount = parsed_amount
        .checked_mul(rate)
        .ok_or("Overflow in rate multiplication")?;

    // Convert to smallest units by multiplying by 10^decimals
    let smallest_units = multiply_by_decimals(crypto_amount, decimals)?;

    // Round UP to a whole base unit. Settlement demands the amount received be
    // at least the invoice amount, so a floored quote converts back to
    // slightly less than the invoice and can never settle even when paid to
    // the unit. Rounding up costs the customer at most one base unit.
    decimal_to_integer_string(smallest_units.ceil())
}

/// Convert a human-readable amount to smallest units (no rate conversion).
///
/// # Arguments
/// * `amount` - Amount in human-readable format (e.g., "1.5" ETH)
/// * `decimals` - Number of decimals for the asset (e.g., 18 for ETH)
///
/// # Returns
/// Amount in smallest units as a string (e.g., "1500000000000000000" wei).
pub(crate) fn convert_human_to_smallest_unit(
    amount: &str,
    decimals: u8,
) -> Result<String, &'static str> {
    // Parse amount
    let parsed_amount: Decimal = amount.parse().map_err(|_| "Invalid amount")?;

    // Validate amount is positive
    if parsed_amount <= Decimal::ZERO {
        return Err("Amount must be positive");
    }

    // Convert to smallest units
    let smallest_units = multiply_by_decimals(parsed_amount, decimals)?;

    decimal_to_integer_string(smallest_units)
}

/// Multiply a decimal by 10^decimals.
pub(crate) fn multiply_by_decimals(value: Decimal, decimals: u8) -> Result<Decimal, &'static str> {
    let ten = Decimal::from(10);
    let mut multiplier = Decimal::ONE;
    for _ in 0..decimals {
        multiplier = multiplier
            .checked_mul(ten)
            .ok_or("Overflow computing multiplier")?;
    }
    value
        .checked_mul(multiplier)
        .ok_or("Overflow in smallest units calculation")
}

/// Convert a Decimal to an integer string (floor, then stringify).
pub(crate) fn decimal_to_integer_string(value: Decimal) -> Result<String, &'static str> {
    let floored = value.floor();

    if floored.is_sign_negative() {
        return Err("Negative amount after conversion");
    }

    // Try to_u128 first for efficient conversion
    match floored.to_u128() {
        Some(n) => Ok(n.to_string()),
        None => {
            // Value too large for u128 - use mantissa extraction
            let normalized = floored.normalize();
            if normalized.scale() > 0 {
                return Err("Unexpected decimal places in conversion result");
            }
            let mantissa = normalized.mantissa();
            if mantissa < 0 {
                return Err("Negative amount after conversion");
            }
            Ok(mantissa.to_string())
        }
    }
}

/// Fetch invoice and verify user has access (admin or store member), AND that
/// the authenticating key (if any) was scoped to `VIEW_INVOICES` on this
/// store. Returns NOT_FOUND for a missing invoice, a non-member, or a key
/// that isn't scoped to read it (prevents enumeration in every case alike).
pub(crate) async fn get_invoice_with_permission<A: SessionService>(
    state: &PgAppState<A>,
    user: &auth::UserInfo,
    key_scope: Option<&[String]>,
    invoice_id: &InvoiceId,
) -> Result<InvoiceData, StatusCode> {
    let invoice = InvoiceReader::get(&*state.data_service, invoice_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let is_member = match &invoice {
        Some(invoice) if user.role != auth::Role::ServerAdmin => state
            .data_service
            .get_user_store(user.id, invoice.store_id)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
            .is_some(),
        _ => false,
    };

    visible_invoice(
        invoice,
        user.role == auth::Role::ServerAdmin,
        is_member,
        key_scope,
    )
}

/// The decision behind [`get_invoice_with_permission`], on facts already
/// loaded. A missing invoice, a non-member and an out-of-scope key all answer
/// the same `NOT_FOUND`, so the answer never says whether an id exists.
pub(crate) fn visible_invoice(
    invoice: Option<InvoiceData>,
    is_admin: bool,
    is_member: bool,
    key_scope: Option<&[String]>,
) -> Result<InvoiceData, StatusCode> {
    let invoice = invoice.ok_or(StatusCode::NOT_FOUND)?;
    if !is_admin && !is_member {
        return Err(StatusCode::NOT_FOUND);
    }
    if !key_grants_store_permission(key_scope, VIEW_INVOICES, invoice.store_id) {
        return Err(StatusCode::NOT_FOUND);
    }
    Ok(invoice)
}

/// Resolve store names for a page of list results.
///
/// The "All Stores" invoice/payment views mix rows from stores the caller may
/// not have in their sidebar (`GET /stores` only returns the caller's own
/// memberships, even for admins), so the name has to come from the server or
/// the row can only show a bare UUID.
///
/// Deduplicates first: a page is at most `limit` rows but usually spans only a
/// handful of stores, so this is a few lookups rather than one per row. A store
/// that fails to load is simply absent from the map — a missing name degrades
/// the column to the store ID, which is not worth failing the whole list over.
pub(crate) async fn resolve_store_names<A: SessionService>(
    state: &PgAppState<A>,
    store_ids: impl IntoIterator<Item = ::types::StoreId>,
) -> std::collections::HashMap<uuid::Uuid, String> {
    use auth::repository::StoreRepository;

    let unique: std::collections::BTreeSet<uuid::Uuid> =
        store_ids.into_iter().map(|id| id.0).collect();

    let mut names = std::collections::HashMap::with_capacity(unique.len());
    for id in unique {
        match StoreRepository::get_store(&*state.data_service, ::types::StoreId(id)).await {
            Ok(Some(store)) => {
                names.insert(id, store.name);
            }
            // A store row that is genuinely absent is not an error worth logging
            // - the column simply falls back to the id.
            Ok(None) => {}
            // A repository error is. Degrading every row on the page to a bare
            // UUID while staying silent gives an operator nothing to go on when
            // someone reports "the store column stopped working".
            Err(e) => tracing::warn!(
                store_id = %id,
                error = %e,
                "could not resolve store name; rows will show a bare store id"
            ),
        }
    }
    names
}

/// Apply token policy filter to payment methods.
///
/// If the policy mode is `Allowlist`, only payment methods matching an entry are kept.
/// If the policy mode is `Blocklist`, payment methods matching an entry are removed.
pub(crate) fn apply_token_policy_filter(
    payment_methods: &mut Vec<data_service::StorePaymentMethod>,
    policy: &data_service::StoreTokenPolicyWithEntries,
) {
    payment_methods.retain(|pm| {
        let chain_id = &pm.chain_id;
        let matches_entry = policy
            .entries
            .iter()
            .any(|e| &e.chain_id == chain_id && e.token_address == pm.token_address);
        match policy.mode {
            ::types::TokenPolicyMode::Allowlist => matches_entry,
            ::types::TokenPolicyMode::Blocklist => !matches_entry,
        }
    });
}

/// The address receipts go to, for one invoice.
///
/// Prefers the dedicated column and falls back to `metadata`. The
/// fallback is for rows written before the migration: those still carry the
/// address inside the blob, and dropping it would silently stop their receipts.
/// New writes populate the column, so the fallback ages out on its own.
pub(crate) fn customer_email_of(invoice: &InvoiceData) -> Option<String> {
    invoice
        .customer_email
        .clone()
        .or_else(|| extract_customer_email(&invoice.metadata))
}

/// Legacy path: read the address out of the metadata blob.
///
/// Only for invoices created before `customer_email` became a column. Do not
/// call this directly on new code paths - use [`customer_email_of`], which
/// prefers the column. Once metadata is encrypted this can only ever
/// return `None` for rows written after that point, which is correct.
pub(crate) fn extract_customer_email(metadata: &Option<serde_json::Value>) -> Option<String> {
    metadata
        .as_ref()
        .and_then(|m| m.get("customer_email").or_else(|| m.get("buyer_email")))
        .and_then(|v| v.as_str())
        .map(String::from)
}

/// Notify the EVM monitor to watch an address, then mark it as notified in the DB.
/// Logs warnings on failure but never errors out — the retry service picks up misses.
/// The EIP-155 number this monitor needs, or `None` with a log line.
///
/// The monitor talks to EVM RPCs; a non-EVM chain cannot be watched by it at
/// all, and silently skipping would leave an invoice that never detects payment.
fn eip155_for_watch(chain_id: &::types::ChainId) -> Option<u64> {
    match chain_id.evm_chain_id() {
        Some(eip155) => Some(eip155),
        None => {
            tracing::error!(%chain_id, "not an EVM chain; cannot watch this address");
            None
        }
    }
}

pub(crate) async fn notify_evm_watch<A: SessionService>(
    state: &PgAppState<A>,
    invoice_id_str: &str,
    payment_address: &str,
    chain_id: &::types::ChainId,
    token_address: Option<&str>,
    address: evm::Address,
    expected_amount: Option<evm::U256>,
    token_contract: Option<evm::Address>,
) {
    let Some(eip155) = eip155_for_watch(chain_id) else {
        return;
    };

    let Some(ref monitor) = state.evm_monitor else {
        return;
    };
    let Ok(invoice_uuid) = uuid::Uuid::parse_str(invoice_id_str) else {
        tracing::error!(id = %invoice_id_str, "Failed to parse invoice ID as UUID");
        return;
    };

    match monitor
        .watch_address_by_chain_id(
            // The monitor is EVM-only and its RPCs take EIP-155 numbers.
            eip155,
            address,
            invoice_uuid,
            expected_amount,
            token_contract,
        )
        .await
    {
        Ok(()) => {
            if let Err(e) = ::types::WatchedAddressWriter::mark_notified(
                &*state.data_service,
                payment_address,
                chain_id,
                token_address,
            )
            .await
            {
                tracing::warn!(
                    address = %payment_address, error = %e,
                    "Failed to mark watch as notified"
                );
            }
        }
        Err(e) => {
            tracing::warn!(
                invoice_id = %invoice_uuid, address = %address, error = %e,
                "Failed to send WatchAddress command, will be retried"
            );
        }
    }
}
