//! Payout/settlement API endpoints.
//!
//! POST /stores/{store_id}/payouts — record a payout the merchant will make.
//! GET  /stores/{store_id}/payouts — List payouts for a store.
//! GET  /stores/{store_id}/payouts/{payout_id} — Get payout details.
//! POST /stores/{store_id}/payouts/{payout_id}/settle — the merchant made it.
//! POST /stores/{store_id}/payouts/{payout_id}/abandon — they will not.
//!
//! **This server never sends funds.** It holds public xpubs and no spending
//! key, so a payout is something the merchant performs from their own wallet
//! and this records. Nothing here signs or broadcasts a transaction, and the
//! row a payout creates was never an instruction the server would act on.
//!
//! Three rules hold everything together, and all three are enforced below
//! rather than assumed: a payout may only be computed from invoices the store
//! in the path owns; an invoice's money may only be claimed once; and a claim
//! can always be released, because a claim that outlives the intent it was
//! protecting refuses every later payout of the same money.

#[cfg(test)]
mod tests;

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use chrono::Utc;
use uuid::Uuid;

use alloy_primitives::U256;
use auth::{SessionService, UserStoreRepository};
use data_service::{InvoiceReader, PaymentReader, PayoutClaimReader, PayoutReader, PayoutWriter};
use types::{PayoutData, PayoutStatus, StoreId};

use super::extractors::{StoreScopedUser, key_grants_store_permission};
use crate::metrics;
use crate::state::PgAppState;
pub use api_types::{CreatePayoutRequest, PayoutListResponse, PayoutResponse};

/// Gate a payout read or write behind a store permission.
///
/// Every sibling module (webhooks, invoices, settings) checks a specific
/// `ethpay.store.can...` permission before a privileged operation; this
/// module used to check only bare store membership, so any role down to
/// Guest - and any API key regardless of scope, since `AuthenticatedUser`
/// carries no key scope at all - could create, settle or abandon a payout.
///
/// `NOT_FOUND`, not `FORBIDDEN`, on refusal: this module's own convention
/// (see `server/src/api/payouts/tests.rs`) is that the difference between
/// "not yours" and "no such row" is itself a fact about another merchant's
/// data, and a permission failure is the same kind of fact as a membership
/// failure.
async fn require_payout_permission<A: SessionService>(
    state: &PgAppState<A>,
    user: &auth::UserInfo,
    key_scope: Option<&[String]>,
    store_id: StoreId,
    permission: &str,
) -> Result<(), StatusCode> {
    if user.role.is_admin() {
        return Ok(());
    }
    let has_permission = state
        .data_service
        .user_has_store_permission(user.id, store_id, permission)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        && key_grants_store_permission(key_scope, permission, store_id);

    if has_permission {
        Ok(())
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}

/// Total of the payments on this chain and asset that may be paid out.
///
/// Reorged and unconfirmed payments are excluded; an unparseable amount is
/// skipped rather than failing the payout, matching the behaviour before
/// chain ids became CAIP-2.
fn sum_payable(
    payments: &[types::PaymentData],
    chain_id: &types::ChainId,
    asset_symbol: &str,
) -> U256 {
    payments
        .iter()
        .filter(|p| {
            p.confirmed_at.is_some()
                && !p.reorged
                && &p.chain_id == chain_id
                && p.asset_symbol == asset_symbol
        })
        .filter_map(|p| p.amount.parse::<U256>().ok())
        .fold(U256::ZERO, |acc, amt| acc + amt)
}

/// Sum what `invoice_ids` are worth, refusing any invoice the store does not own.
///
/// The caller supplies the invoice ids and the destination address; membership
/// of the store in the path is all that has been checked by this point. So each
/// invoice is loaded and matched against that store — an id the store does not
/// own contributes nothing and stops the request.
///
/// The refusal is 404, the same answer as an invoice that does not exist. A 403
/// would separate "not yours" from "no such invoice", and that difference is
/// itself another merchant's data.
async fn payable_total<R>(
    reader: &R,
    store_id: StoreId,
    invoice_ids: &[String],
    chain_id: &types::ChainId,
    asset_symbol: &str,
) -> Result<U256, StatusCode>
where
    R: InvoiceReader + PaymentReader,
{
    let mut total = U256::ZERO;

    for invoice_id_str in invoice_ids {
        let invoice_id = types::InvoiceId::from_string(invoice_id_str.clone());

        let invoice = InvoiceReader::get(reader, &invoice_id)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
            .ok_or(StatusCode::NOT_FOUND)?;

        if invoice.store_id != store_id {
            tracing::warn!(
                store_id = %store_id,
                "Payout named an invoice that belongs to another store"
            );
            return Err(StatusCode::NOT_FOUND);
        }

        let payments = PaymentReader::get_valid_for_invoice(reader, &invoice_id)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

        total += sum_payable(&payments, chain_id, asset_symbol);
    }

    Ok(total)
}

/// Refuse a payout over invoices an existing payout already claims.
///
/// Without this the same invoices can be paid out again and again, each payout
/// recomputing the full total from the same payments. Answered 409: the request
/// is well formed, the state is what refuses it.
async fn reject_claimed_invoices<R>(
    reader: &R,
    store_id: StoreId,
    invoice_ids: &[String],
) -> Result<(), StatusCode>
where
    R: PayoutClaimReader,
{
    let claimed = reader
        .invoice_ids_already_claimed(store_id, invoice_ids)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "Failed to read existing payout claims");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    if claimed.is_empty() {
        return Ok(());
    }

    tracing::warn!(
        store_id = %store_id,
        claimed = claimed.len(),
        "Payout refused: these invoices are already claimed by another payout"
    );
    Err(StatusCode::CONFLICT)
}

/// Load a payout, refusing one that belongs to a different store.
///
/// The path carries both ids and only the store id is checked by the membership
/// gate, so the payout itself must be matched to it. Mismatch is 404 for the
/// same reason as in [`payable_total`].
async fn payout_for_store<R>(
    reader: &R,
    store_id: StoreId,
    payout_id: Uuid,
) -> Result<PayoutData, StatusCode>
where
    R: PayoutReader,
{
    let payout = PayoutReader::get_payout(reader, payout_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    if payout.store_id != store_id {
        tracing::warn!(
            store_id = %store_id,
            "Payout lookup refused: the payout belongs to another store"
        );
        return Err(StatusCode::NOT_FOUND);
    }

    Ok(payout)
}

/// Initiate a payout — record an intent to sweep funds to a merchant wallet.
///
/// This writes a payout row and nothing else. No transaction is signed or
/// broadcast anywhere in this deployment: the server holds only public xpubs, so
/// the record stays `Pending` until something outside this process acts on it.
pub async fn create_payout<A>(
    StoreScopedUser(user, key_scope): StoreScopedUser,
    State(state): State<PgAppState<A>>,
    Path(store_id): Path<Uuid>,
    Json(body): Json<CreatePayoutRequest>,
) -> Result<Json<PayoutResponse>, StatusCode>
where
    A: SessionService + 'static,
{
    let store_id = StoreId(store_id);

    require_payout_permission(
        &state,
        &user,
        key_scope.as_deref(),
        store_id,
        auth::Policies::STORE_MODIFY_INVOICES,
    )
    .await?;

    // Validate destination address is not empty
    if body.destination_address.trim().is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }

    // Validate invoice_ids are not empty
    if body.invoice_ids.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }

    // Ownership first, then whether the money is still unclaimed: an invoice
    // this store does not own is a 404 whatever its payout history says.
    let total_amount = payable_total(
        &*state.data_service,
        store_id,
        &body.invoice_ids,
        &body.chain_id,
        &body.asset_symbol,
    )
    .await?;

    reject_claimed_invoices(&*state.data_service, store_id, &body.invoice_ids).await?;

    if total_amount.is_zero() {
        tracing::warn!(store_id = %store_id, "No confirmed payments found for payout");
        return Err(StatusCode::BAD_REQUEST);
    }

    let asset_type = if body.token_address.is_some() {
        "erc20"
    } else {
        "native"
    }
    .to_string();

    let payout = PayoutData {
        id: Uuid::new_v4(),
        store_id,
        invoice_ids: body.invoice_ids,
        destination_address: body.destination_address,
        chain_id: body.chain_id.clone(),
        asset_type,
        asset_symbol: body.asset_symbol.clone(),
        token_address: body.token_address,
        amount: total_amount.to_string(),
        tx_hash: None,
        status: PayoutStatus::Pending,
        fee_amount: None,
        error_message: None,
        created_at: Utc::now(),
        confirmed_at: None,
    };

    PayoutWriter::create_payout(&*state.data_service, &payout)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "Failed to create payout record");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    metrics::record_payout_initiated(&body.chain_id, &body.asset_symbol);

    tracing::info!(
        payout_id = %payout.id,
        store_id = %store_id,
        amount = %payout.amount,
        destination = %payout.destination_address,
        "Payout created"
    );

    Ok(Json(payout.into()))
}

/// List payouts for a store.
pub async fn list_payouts<A>(
    StoreScopedUser(user, key_scope): StoreScopedUser,
    State(state): State<PgAppState<A>>,
    Path(store_id): Path<Uuid>,
) -> Result<Json<PayoutListResponse>, StatusCode>
where
    A: SessionService + 'static,
{
    let store_id = StoreId(store_id);

    require_payout_permission(
        &state,
        &user,
        key_scope.as_deref(),
        store_id,
        auth::Policies::STORE_VIEW_PAYMENTS,
    )
    .await?;

    let (total, payouts) =
        PayoutReader::get_payouts_for_store(&*state.data_service, store_id, 50, 0)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(PayoutListResponse {
        total,
        payouts: payouts.into_iter().map(Into::into).collect::<Vec<_>>(),
    }))
}

/// Get a specific payout.
pub async fn get_payout<A>(
    StoreScopedUser(user, key_scope): StoreScopedUser,
    State(state): State<PgAppState<A>>,
    Path((store_id, payout_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<PayoutResponse>, StatusCode>
where
    A: SessionService + 'static,
{
    let store_id = StoreId(store_id);

    require_payout_permission(
        &state,
        &user,
        key_scope.as_deref(),
        store_id,
        auth::Policies::STORE_VIEW_PAYMENTS,
    )
    .await?;

    let payout = payout_for_store(&*state.data_service, store_id, payout_id).await?;

    Ok(Json(payout.into()))
}

/// What a merchant sends when recording a payout they made themselves.
#[derive(Debug, serde::Deserialize)]
pub struct SettlePayoutRequest {
    /// The transaction the merchant broadcast from their own wallet.
    pub tx_hash: String,
}

/// Why a payout is being abandoned.
#[derive(Debug, Default, serde::Deserialize)]
pub struct AbandonPayoutRequest {
    /// Free text, stored on the payout so the next reader knows why.
    pub reason: Option<String>,
}

/// Record that the merchant has made this payout from their own wallet.
///
/// This server never sends funds. It holds public xpubs only and has no
/// spending key, so a payout is something the merchant performs and this
/// records — the row was never an instruction the server would act on.
///
/// Moves the payout to `Confirmed` and stores the transaction hash the
/// merchant supplies. Deliberately not verified against the chain: the server
/// cannot know which wallet the merchant paid from, and refusing a hash it
/// cannot corroborate would leave the merchant unable to close out a payout
/// they genuinely made. The hash is the merchant's own record.
///
/// Only a `Pending` payout can be settled. Settling one twice is refused
/// rather than silently overwriting the first transaction hash.
pub async fn settle_payout<A>(
    StoreScopedUser(user, key_scope): StoreScopedUser,
    State(state): State<PgAppState<A>>,
    Path((store_id, payout_id)): Path<(Uuid, Uuid)>,
    Json(body): Json<SettlePayoutRequest>,
) -> Result<Json<PayoutResponse>, StatusCode>
where
    A: SessionService + 'static,
{
    let store_id = StoreId(store_id);

    require_payout_permission(
        &state,
        &user,
        key_scope.as_deref(),
        store_id,
        auth::Policies::STORE_MODIFY_INVOICES,
    )
    .await?;

    let tx_hash = body.tx_hash.trim();
    if tx_hash.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }

    let payout = payout_for_store(&*state.data_service, store_id, payout_id).await?;

    if payout.status != PayoutStatus::Pending {
        tracing::warn!(
            payout_id = %payout_id,
            status = ?payout.status,
            "Settle refused: only a pending payout can be settled"
        );
        return Err(StatusCode::CONFLICT);
    }

    PayoutWriter::update_payout_status(
        &*state.data_service,
        payout_id,
        PayoutStatus::Confirmed,
        Some(tx_hash),
        None,
        None,
    )
    .await
    .map_err(|e| {
        tracing::error!(error = %e, "Failed to settle payout");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    tracing::info!(
        payout_id = %payout_id,
        store_id = %store_id,
        tx_hash = %tx_hash,
        "Payout settled by the merchant"
    );

    let settled = payout_for_store(&*state.data_service, store_id, payout_id).await?;
    Ok(Json(settled.into()))
}

/// Abandon a payout the merchant is not going to make.
///
/// Creating a payout claims its invoices so the same money cannot be paid out
/// twice, and that claim covers every payout that is not `failed`. Without a
/// way to release it, a payout recorded by mistake would hold those invoices
/// for good and refuse every later payout of the same money — the claim
/// outliving the intent it was protecting.
///
/// Moves the payout to `Failed`, which is what the claim query already treats
/// as released.
pub async fn abandon_payout<A>(
    StoreScopedUser(user, key_scope): StoreScopedUser,
    State(state): State<PgAppState<A>>,
    Path((store_id, payout_id)): Path<(Uuid, Uuid)>,
    body: Option<Json<AbandonPayoutRequest>>,
) -> Result<Json<PayoutResponse>, StatusCode>
where
    A: SessionService + 'static,
{
    let store_id = StoreId(store_id);

    require_payout_permission(
        &state,
        &user,
        key_scope.as_deref(),
        store_id,
        auth::Policies::STORE_MODIFY_INVOICES,
    )
    .await?;

    let payout = payout_for_store(&*state.data_service, store_id, payout_id).await?;

    if payout.status != PayoutStatus::Pending {
        tracing::warn!(
            payout_id = %payout_id,
            status = ?payout.status,
            "Abandon refused: only a pending payout can be abandoned"
        );
        return Err(StatusCode::CONFLICT);
    }

    let reason = body
        .and_then(|Json(b)| b.reason)
        .unwrap_or_else(|| "abandoned by the merchant".to_string());

    PayoutWriter::update_payout_status(
        &*state.data_service,
        payout_id,
        PayoutStatus::Failed,
        None,
        None,
        Some(&reason),
    )
    .await
    .map_err(|e| {
        tracing::error!(error = %e, "Failed to abandon payout");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    tracing::info!(
        payout_id = %payout_id,
        store_id = %store_id,
        reason = %reason,
        "Payout abandoned; its invoices are claimable again"
    );

    let abandoned = payout_for_store(&*state.data_service, store_id, payout_id).await?;
    Ok(Json(abandoned.into()))
}
