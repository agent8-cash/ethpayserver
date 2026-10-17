//! Store payment method CRUD endpoints: list, create, get, update, delete.
//!
//! The audit that preceded this gate - which existing rows named a chain
//! with no adapter - is deliberately not reproduced here: this file ships in
//! a public repository, and the result would go stale the moment a new row
//! appears.

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use uuid::Uuid;

use ::types::ChainId;
use auth::repository::StoreRepository;
use auth::{ServerSettings, ServerSettingsRepository, SessionService, StoreId};
use data_service::{self, StorePaymentMethodReader, StorePaymentMethodWriter};
use evm::{validate_xpub, xpub_depth_problem};

use super::super::extractors::StoreScopedUser;
use super::{ApiErr, repository_error, require_store_settings_permission};
use crate::state::PgAppState;
pub use api_types::{
    CreatePaymentMethodRequest, PaymentMethodResponse, UpdatePaymentMethodRequest,
};

/// Which side of a request `chain_has_no_adapter` is checking: a chain a
/// request is trying to introduce, or a chain already sitting on a stored
/// row. Only matters when `settings` is `None` - see `chain_has_no_adapter`.
pub(crate) enum ChainCheckContext {
    /// `create`'s `req.chain_id`, or any other chain a request wants this
    /// server to start treating as real. Nothing has vouched for it yet.
    New,
    /// `update`'s `existing.chain_id` - a chain that is already a row in the
    /// database. `UpdatePaymentMethodRequest` carries no chain id of its own,
    /// so this can never be a value the request chose.
    Existing,
}

/// Whether the server has a registered adapter for this chain.
///
/// When a settings row exists, this is membership in
/// `enabled_chain_ids` - the operator's list of chains something on this
/// server actually watches - not "is this an EVM chain", because that would
/// hardcode today's only adapter into the check. A Tron adapter registers by
/// the operator adding `tron:...` to that list; this predicate does not
/// change.
///
/// `settings` is `None` when nobody has ever written a `server_settings`
/// row - confirmed true of the live testnet database, and unverified but
/// plausibly also true of mainnet (see the ticket's audit). Nothing in this
/// codebase says which environment a given binary is running as, so the
/// fallback below cannot use one fixed chain set for both callers - it
/// answers a different question depending on `context`:
///
/// - `New`: checked against `evm::testnet`'s registry only. A request
///   introducing a chain id is exactly the case that must not guess - an
///   unconfigured mainnet deployment must refuse every chain until an
///   operator writes `enabled_chain_ids` (mainnet chain ids are never
///   testnet chain ids, so this still refuses them), while an unconfigured
///   testnet deployment keeps accepting its one real chain, Sepolia
///   (`eip155:11155111`), without requiring that same operator step first.
///   Earlier versions of this fallback used `ServerSettings::default()`
///   (a Rust-side EVM-*mainnet* list that does not contain Sepolia, so it
///   turned "reject Tron" into "reject testnet's only real chain"),
///   `is_evm()` (accepted any invented `eip155:<n>`), and
///   `evm::get_any_chain_config` (accepted a mainnet chain id, e.g.
///   `eip155:1`, on the Sepolia-only testnet deployment - the exact hole
///   this ticket exists to close, reopened for mainnet chains instead of
///   Tron). All three are wrong for the same reason: none of them can tell
///   testnet and mainnet apart, so each was safe for one and not the other.
/// - `Existing`: checked against the full compiled registry
///   (`evm::get_any_chain_config`, mainnet and testnet). This case is never
///   a value the request chose, so there is no "which environment is this"
///   question to get wrong - the only thing left to catch is a namespace
///   with no adapter anywhere at all (Tron). Using the `New` behavior here
///   too would 400 every update to an already-working row on an
///   unconfigured deployment - re-enabling it, rotating its xpub - the
///   moment `server_settings` is empty, which is new collateral damage this
///   ticket does not need to cause. A stored chain id that the compiled
///   registry recognizes was, by definition, watchable when it was written.
///
/// Either way this is not a substitute for `enabled_chain_ids`: a `Some`
/// settings row always wins, and once an operator writes one, every chain
/// not in it is refused regardless of what `evm` recognizes.
///
/// Without this gate at all a merchant can register e.g. `tron:728126428`,
/// which still derives a secp256k1 address (the same curve as EVM) but that
/// address is never watched - the invoice can be paid and is never marked so.
pub(crate) fn chain_has_no_adapter(
    chain_id: &ChainId,
    settings: Option<&ServerSettings>,
    context: ChainCheckContext,
) -> bool {
    match settings {
        Some(settings) => !settings.enabled_chain_ids.contains(chain_id),
        None => {
            let evm_chain_id = chain_id.evm_chain_id();
            match context {
                ChainCheckContext::New => evm_chain_id
                    .and_then(evm::testnet::get_testnet_config)
                    .is_none(),
                ChainCheckContext::Existing => {
                    evm_chain_id.and_then(evm::get_any_chain_config).is_none()
                }
            }
        }
    }
}

/// The 400 both `create_payment_method` and `update_payment_method` return
/// when `chain_has_no_adapter` refuses a chain - one place so the status code
/// and the exact wording (`unsupported_chain`, naming the chain) can't drift
/// between the two call sites.
pub(crate) fn unsupported_chain_error(chain_id: &ChainId) -> ApiErr {
    (
        StatusCode::BAD_REQUEST,
        format!("unsupported_chain: no adapter is registered for {chain_id}"),
    )
        .into()
}

/// Whether an update to a payment method should still be checked against
/// `chain_has_no_adapter`.
///
/// The check only ever fires for a legacy row that predates this ticket -
/// `UpdatePaymentMethodRequest` has no `chain_id`, so a fresh row can never
/// fail it. If it also blocked `enabled: false` on such a row, the only way
/// to turn one off would be direct database surgery, which is exactly what
/// the ticket's audit step is for someone to be able to avoid. Disabling
/// only reduces exposure, so it is let through unconditionally; anything
/// else on a bad row (re-enabling it, rotating its xpub) is still refused.
pub(crate) fn update_should_check_chain(requested_enabled: Option<bool>) -> bool {
    requested_enabled != Some(false)
}

/// List payment methods for a store.
#[utoipa::path(
    get,
    path = "/stores/{store_id}/payment-methods",
    tag = "stores",
    security(("bearer_auth" = [])),
    params(
        ("store_id" = Uuid, Path, description = "Store ID")
    ),
    responses(
        (status = 200, description = "List of payment methods", body = Vec<PaymentMethodResponse>),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Insufficient permissions"),
    )
)]
pub async fn list_payment_methods<A>(
    StoreScopedUser(user, key_scope): StoreScopedUser,
    State(state): State<PgAppState<A>>,
    Path(store_id): Path<Uuid>,
) -> Result<Json<Vec<PaymentMethodResponse>>, StatusCode>
where
    A: SessionService + 'static,
{
    require_store_settings_permission(&state, &user, key_scope.as_deref(), store_id).await?;

    let methods = StorePaymentMethodReader::get_payment_methods(&*state.data_service, store_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(methods.into_iter().map(|m| m.into()).collect()))
}

/// Create a payment method for a store.
#[utoipa::path(
    post,
    path = "/stores/{store_id}/payment-methods",
    tag = "stores",
    security(("bearer_auth" = [])),
    params(
        ("store_id" = Uuid, Path, description = "Store ID")
    ),
    request_body = CreatePaymentMethodRequest,
    responses(
        (status = 201, description = "Payment method created", body = PaymentMethodResponse),
        (status = 409, description = "That xpub is registered to another account"),
        (status = 400, description = "Invalid request"),
        (status = 422, description = "Malformed body — e.g. a chain id that is not CAIP-2"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Insufficient permissions"),
        (status = 404, description = "Store not found"),
    )
)]
pub async fn create_payment_method<A>(
    StoreScopedUser(user, key_scope): StoreScopedUser,
    State(state): State<PgAppState<A>>,
    Path(store_id): Path<Uuid>,
    Json(req): Json<CreatePaymentMethodRequest>,
) -> Result<(StatusCode, Json<PaymentMethodResponse>), ApiErr>
where
    A: SessionService + 'static,
{
    require_store_settings_permission(&state, &user, key_scope.as_deref(), store_id).await?;

    // A key is optional now: omitted means "use the one this store already
    // resolves to", so a merchant pastes it once rather than per chain, per
    // token and per store. When one IS given it still has to be a real
    // extended PUBLIC key - `validate_xpub` refuses an `xprv` on the version
    // byte, which is what keeps this non-custodial even if someone pastes the
    // wrong line out of their wallet.
    if let Some(ref xpub) = req.xpub
        && !validate_xpub(xpub)
    {
        return Err(StatusCode::BAD_REQUEST.into());
    }
    if let Some(ref xpub) = req.xpub
        && let Some(reason) = xpub_depth_problem(xpub)
    {
        return Err((StatusCode::BAD_REQUEST, reason).into());
    }

    // Refuse a chain nothing here can watch. See `chain_has_no_adapter` -
    // otherwise the option gets an address (the same curve derives one for
    // any namespace) and quotes a customer who can pay it while nothing
    // notices the money arrived.
    let settings = state
        .data_service
        .get_server_settings()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if chain_has_no_adapter(&req.chain_id, settings.as_ref(), ChainCheckContext::New) {
        return Err(unsupported_chain_error(&req.chain_id));
    }

    // Verify store exists
    let _ = state
        .data_service
        .get_store(StoreId(store_id))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    let method = StorePaymentMethodWriter::create_payment_method(
        &*state.data_service,
        store_id,
        &req.chain_id,
        req.token_address.as_deref(),
        &req.asset_symbol,
        req.decimals,
        req.xpub.as_deref(),
    )
    .await
    .map_err(repository_error)?;

    Ok((StatusCode::CREATED, Json(method.into())))
}

/// Get a specific payment method.
#[utoipa::path(
    get,
    path = "/stores/{store_id}/payment-methods/{method_id}",
    tag = "stores",
    security(("bearer_auth" = [])),
    params(
        ("store_id" = Uuid, Path, description = "Store ID"),
        ("method_id" = Uuid, Path, description = "Payment method ID")
    ),
    responses(
        (status = 200, description = "Payment method details", body = PaymentMethodResponse),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Insufficient permissions"),
        (status = 404, description = "Payment method not found"),
    )
)]
pub async fn get_payment_method<A>(
    StoreScopedUser(user, key_scope): StoreScopedUser,
    State(state): State<PgAppState<A>>,
    Path((store_id, method_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<PaymentMethodResponse>, StatusCode>
where
    A: SessionService + 'static,
{
    require_store_settings_permission(&state, &user, key_scope.as_deref(), store_id).await?;

    let method = StorePaymentMethodReader::get_payment_method(&*state.data_service, method_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    // Verify it belongs to this store
    if method.store_id != store_id {
        return Err(StatusCode::NOT_FOUND);
    }

    Ok(Json(method.into()))
}

/// Update a payment method.
#[utoipa::path(
    put,
    path = "/stores/{store_id}/payment-methods/{method_id}",
    tag = "stores",
    security(("bearer_auth" = [])),
    params(
        ("store_id" = Uuid, Path, description = "Store ID"),
        ("method_id" = Uuid, Path, description = "Payment method ID")
    ),
    request_body = UpdatePaymentMethodRequest,
    responses(
        (status = 200, description = "Payment method updated", body = PaymentMethodResponse),
        (status = 409, description = "That xpub is registered to another account"),
        (status = 400, description = "Invalid request"),
        (status = 422, description = "Malformed body — e.g. a chain id that is not CAIP-2"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Insufficient permissions"),
        (status = 404, description = "Payment method not found"),
    )
)]
pub async fn update_payment_method<A>(
    StoreScopedUser(user, key_scope): StoreScopedUser,
    State(state): State<PgAppState<A>>,
    Path((store_id, method_id)): Path<(Uuid, Uuid)>,
    Json(req): Json<UpdatePaymentMethodRequest>,
) -> Result<Json<PaymentMethodResponse>, ApiErr>
where
    A: SessionService + 'static,
{
    require_store_settings_permission(&state, &user, key_scope.as_deref(), store_id).await?;

    // Validate xpub if provided
    if let Some(ref xpub) = req.xpub
        && !validate_xpub(xpub)
    {
        return Err(StatusCode::BAD_REQUEST.into());
    }
    if let Some(ref xpub) = req.xpub
        && let Some(reason) = xpub_depth_problem(xpub)
    {
        return Err((StatusCode::BAD_REQUEST, reason).into());
    }

    // Verify method exists and belongs to this store
    let existing = StorePaymentMethodReader::get_payment_method(&*state.data_service, method_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    if existing.store_id != store_id {
        return Err(StatusCode::NOT_FOUND.into());
    }

    // Same gate as creation, against the chain already stored on this method -
    // `UpdatePaymentMethodRequest` carries no chain_id of its own, so there is
    // nothing to validate on the request. This only bites a row from before
    // this check existed (this file's history carries the audit); a
    // fresh row can never have an unsupported chain. `ChainCheckContext::Existing`
    // (not `::New`) because this chain id was never a value this request
    // chose - see `chain_has_no_adapter`'s doc comment for why the two
    // contexts fall back differently when there is no settings row.
    if update_should_check_chain(req.enabled) {
        let settings = state
            .data_service
            .get_server_settings()
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        if chain_has_no_adapter(
            &existing.chain_id,
            settings.as_ref(),
            ChainCheckContext::Existing,
        ) {
            return Err(unsupported_chain_error(&existing.chain_id));
        }
    }

    let method = StorePaymentMethodWriter::update_payment_method(
        &*state.data_service,
        method_id,
        req.enabled,
        req.xpub.as_deref(),
    )
    .await
    .map_err(repository_error)?;

    Ok(Json(method.into()))
}

/// Delete a payment method.
#[utoipa::path(
    delete,
    path = "/stores/{store_id}/payment-methods/{method_id}",
    tag = "stores",
    security(("bearer_auth" = [])),
    params(
        ("store_id" = Uuid, Path, description = "Store ID"),
        ("method_id" = Uuid, Path, description = "Payment method ID")
    ),
    responses(
        (status = 204, description = "Payment method deleted"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Insufficient permissions"),
        (status = 404, description = "Payment method not found"),
    )
)]
pub async fn delete_payment_method<A>(
    StoreScopedUser(user, key_scope): StoreScopedUser,
    State(state): State<PgAppState<A>>,
    Path((store_id, method_id)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode, StatusCode>
where
    A: SessionService + 'static,
{
    require_store_settings_permission(&state, &user, key_scope.as_deref(), store_id).await?;

    // Verify method exists and belongs to this store
    let existing = StorePaymentMethodReader::get_payment_method(&*state.data_service, method_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    if existing.store_id != store_id {
        return Err(StatusCode::NOT_FOUND);
    }

    StorePaymentMethodWriter::delete_payment_method(&*state.data_service, method_id)
        .await
        .map_err(|e| match e {
            data_service::RepositoryError::NotFound(_) => StatusCode::NOT_FOUND,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        })?;

    Ok(StatusCode::NO_CONTENT)
}
