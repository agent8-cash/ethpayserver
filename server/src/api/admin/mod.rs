//! Admin API endpoints.
//!
//! All endpoints require `AdminAuth` (ServerAdmin role). Covers user
//! management and server-wide settings; `deletion` is account/store
//! hard-delete, `plugins` is the plugin lifecycle (install, upgrade, enable,
//! disable, uninstall, and its audit trail) - the only two parts of this
//! surface large enough to need their own module.

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use ::types::ChainId;
use auth::{
    Role, ServerSettings, ServerSettingsRepository, SessionService, UserId, UserRepository,
};
use data_service::MerchantDirectoryReader;

pub mod deletion;
pub mod plugins;

use super::extractors::{AdminAuth, MerchantReader};
use crate::state::PgAppState;
pub use api_types::{
    AdminUserInfo, ServerSettingsResponse, UpdateRoleRequest, UpdateServerSettingsRequest,
    UserListResponse,
};
pub use deletion::{E2E_STORE_OWNER_ID, delete_user_account, hard_delete_store, list_user_stores};

// ============================================================================
// Types
// ============================================================================

/// Query params for user listing.
#[derive(Debug, Deserialize)]
pub struct ListUsersParams {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

/// Whether this boot has every plugin disabled.
///
/// Not in `api-types`: the full plugin admin surface a future page will
/// build (listing plugins and disabling them individually) belongs in the
/// shared contract once it exists. This is the minimal, honest slice of it
/// that exists today - see `payserver-client`'s `src/api/types/local.rs` for
/// the same "hand-mirror until it earns a shared contract" convention.
///
/// Consumed by `payserver-client`'s `AdminTab`
/// (`src/pages/settings/admin.rs`), which fetches this endpoint and shows a
/// banner when `safe_mode` is true - that UI ships in the same ticket's
/// `payserver-client` PR, not this repository.
#[derive(Debug, Serialize, ToSchema)]
pub struct SafeModeResponse {
    /// True when `ETHPAY_DISABLE_PLUGINS`/`--disable-plugins` was set at
    /// boot. Plugins are disabled, not uninstalled - their files and data are
    /// untouched, and clearing the flag on the next boot restores them.
    pub safe_mode: bool,
    /// True when something on this server can use the operator store: at
    /// least one installed plugin is enabled and safe mode is off. The
    /// operator store only decides where a plugin's invoices are issued and
    /// which store's payments a plugin is told about, so on a server with no
    /// running plugin the setting does nothing and a client should not offer
    /// it.
    pub operator_store_available: bool,
}

/// The safe-mode response for a server, read from its installed plugins.
///
/// A failed read reports the store as unavailable rather than failing the
/// request: `safe_mode` is known without the database, and a client showing
/// its safe-mode warning must still get it when the plugin list cannot be
/// read.
async fn safe_mode_response<R>(reader: &R, safe_mode: bool) -> SafeModeResponse
where
    R: data_service::InstalledPluginReader + ?Sized,
{
    let enabled = match reader.list_installed_plugins().await {
        Ok(installed) => installed.iter().map(|p| p.enabled).collect::<Vec<_>>(),
        Err(e) => {
            tracing::warn!(error = %e, "could not list plugins for the safe-mode response");
            Vec::new()
        }
    };
    SafeModeResponse {
        safe_mode,
        operator_store_available: operator_store_available(safe_mode, enabled),
    }
}

/// Whether an operator store has any consumer on this server.
///
/// Any enabled plugin counts, not only a billing one: the host hands the
/// operator store to every loaded plugin (invoice issuing and payment
/// reports are host-wide), and the server cannot tell what a plugin is for.
/// Based on the installed rows rather than what loaded at boot, so a plugin
/// that was installed or enabled after boot (and takes the store up on the
/// next restart, like the store itself) still makes the setting reachable.
fn operator_store_available(safe_mode: bool, enabled: impl IntoIterator<Item = bool>) -> bool {
    !safe_mode && enabled.into_iter().any(|e| e)
}

// ============================================================================
// Handlers
// ============================================================================

/// List all users (paginated).
#[utoipa::path(
    get,
    path = "/admin/users",
    tag = "admin",
    security(("bearer_auth" = [])),
    params(
        ("limit" = Option<i64>, Query, description = "Max results (default 50)"),
        ("offset" = Option<i64>, Query, description = "Offset for pagination"),
    ),
    responses(
        (status = 200, description = "Paginated user list", body = UserListResponse),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Admin access required"),
    )
)]
pub async fn list_users<A>(
    AdminAuth(_admin): AdminAuth,
    Query(params): Query<ListUsersParams>,
    State(state): State<PgAppState<A>>,
) -> Result<Json<UserListResponse>, StatusCode>
where
    A: SessionService + 'static,
{
    let limit = params.limit.unwrap_or(50).min(200);
    let offset = params.offset.unwrap_or(0);

    let ds = &*state.data_service;

    let users = UserRepository::list_users(ds, offset, limit)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let total = UserRepository::count_users(ds)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let admin_users: Vec<AdminUserInfo> = users
        .iter()
        .map(|u| AdminUserInfo {
            id: u.id.0.to_string(),
            email: u.email.clone(),
            primary_wallet_address: u.primary_wallet_address.clone(),
            role: u.role.as_str().to_string(),
            created_at: u.created_at,
            last_login_at: u.last_login_at,
            locked_until: u.locked_until,
        })
        .collect();

    Ok(Json(UserListResponse {
        users: admin_users,
        total,
        offset,
        limit,
    }))
}

/// One store in the server-wide listing.
///
/// Only what `data_service::MerchantStore` carries: identifiers, name and
/// archived flag, no settings or wallet data.
#[derive(Debug, Serialize, ToSchema)]
pub struct MerchantStoreEntry {
    pub id: String,
    pub name: String,
    pub owner_id: String,
    pub archived: bool,
}

/// List every store on the server (paginated), oldest first.
///
/// Read-only, so unlike the rest of `/admin` it also accepts an API key
/// scoped to `ethpay.server.canviewusers` (owned by a server admin). That
/// lets an integration that only needs to enumerate merchants hold a key
/// that can do nothing else on the admin surface.
#[utoipa::path(
    get,
    path = "/admin/stores",
    tag = "admin",
    security(("bearer_auth" = [])),
    params(
        ("limit" = Option<i64>, Query, description = "Max results (default 50, max 200)"),
        ("offset" = Option<i64>, Query, description = "Offset for pagination"),
    ),
    responses(
        (status = 200, description = "Stores on this server", body = Vec<MerchantStoreEntry>),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Admin access, or a key scoped to read the merchant listing, required"),
    )
)]
pub async fn list_stores<A>(
    MerchantReader(_reader): MerchantReader,
    Query(params): Query<ListUsersParams>,
    State(state): State<PgAppState<A>>,
) -> Result<Json<Vec<MerchantStoreEntry>>, StatusCode>
where
    A: SessionService + 'static,
{
    let limit = params.limit.unwrap_or(50).clamp(1, 200);
    let offset = params.offset.unwrap_or(0).max(0);

    let stores = MerchantDirectoryReader::list_stores(&*state.data_service, offset, limit)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(
        stores
            .into_iter()
            .map(|s| MerchantStoreEntry {
                id: s.id.0.to_string(),
                name: s.name,
                owner_id: s.owner_id.0.to_string(),
                archived: s.archived,
            })
            .collect(),
    ))
}

/// Change a user's role.
///
/// Guard: cannot demote the last remaining ServerAdmin.
#[utoipa::path(
    patch,
    path = "/admin/users/{id}/role",
    tag = "admin",
    security(("bearer_auth" = [])),
    params(("id" = String, Path, description = "User ID")),
    request_body = UpdateRoleRequest,
    responses(
        (status = 200, description = "Role updated"),
        (status = 400, description = "Invalid role or last admin"),
        (status = 422, description = "Malformed body — e.g. a chain id that is not CAIP-2"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Admin access required"),
        (status = 404, description = "User not found"),
    )
)]
pub async fn update_user_role<A>(
    AdminAuth(_admin): AdminAuth,
    Path(user_id): Path<String>,
    State(state): State<PgAppState<A>>,
    Json(body): Json<UpdateRoleRequest>,
) -> Result<StatusCode, (StatusCode, &'static str)>
where
    A: SessionService + 'static,
{
    let new_role: Role = body
        .role
        .parse()
        .map_err(|_| (StatusCode::BAD_REQUEST, "Invalid role"))?;

    let uid = uuid::Uuid::parse_str(&user_id)
        .map_err(|_| (StatusCode::BAD_REQUEST, "Invalid user ID"))?;
    let uid = UserId(uid);

    let ds = &*state.data_service;

    let mut user = ds
        .get_user(uid)
        .await
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "Database error"))?
        .ok_or((StatusCode::NOT_FOUND, "User not found"))?;

    // Guard: cannot demote the last ServerAdmin
    if user.role == Role::ServerAdmin && new_role != Role::ServerAdmin {
        let all_users = UserRepository::list_users(ds, 0, 10000)
            .await
            .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "Database error"))?;
        let admin_count = all_users
            .iter()
            .filter(|u| u.role == Role::ServerAdmin)
            .count();
        if admin_count <= 1 {
            return Err((
                StatusCode::BAD_REQUEST,
                "Cannot demote the last server admin",
            ));
        }
    }

    user.role = new_role;
    ds.update_user(&user)
        .await
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "Database error"))?;

    Ok(StatusCode::OK)
}

/// Lock a user account (set locked_until to far future).
#[utoipa::path(
    post,
    path = "/admin/users/{id}/lock",
    tag = "admin",
    security(("bearer_auth" = [])),
    params(("id" = String, Path, description = "User ID")),
    responses(
        (status = 200, description = "User locked"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Admin access required"),
        (status = 404, description = "User not found"),
    )
)]
pub async fn lock_user<A>(
    AdminAuth(_admin): AdminAuth,
    Path(user_id): Path<String>,
    State(state): State<PgAppState<A>>,
) -> Result<StatusCode, (StatusCode, &'static str)>
where
    A: SessionService + 'static,
{
    let uid = uuid::Uuid::parse_str(&user_id)
        .map_err(|_| (StatusCode::BAD_REQUEST, "Invalid user ID"))?;
    let uid = UserId(uid);

    // Lock until year 9999 (effectively permanent)
    let far_future = chrono::DateTime::parse_from_rfc3339("9999-12-31T23:59:59Z")
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "Date parse error"))?
        .with_timezone(&Utc);

    state
        .data_service
        .lock_user(uid, far_future)
        .await
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "Failed to lock user"))?;

    Ok(StatusCode::OK)
}

/// Unlock a user account.
#[utoipa::path(
    post,
    path = "/admin/users/{id}/unlock",
    tag = "admin",
    security(("bearer_auth" = [])),
    params(("id" = String, Path, description = "User ID")),
    responses(
        (status = 200, description = "User unlocked"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Admin access required"),
        (status = 404, description = "User not found"),
    )
)]
pub async fn unlock_user<A>(
    AdminAuth(_admin): AdminAuth,
    Path(user_id): Path<String>,
    State(state): State<PgAppState<A>>,
) -> Result<StatusCode, (StatusCode, &'static str)>
where
    A: SessionService + 'static,
{
    let uid = uuid::Uuid::parse_str(&user_id)
        .map_err(|_| (StatusCode::BAD_REQUEST, "Invalid user ID"))?;
    let uid = UserId(uid);

    state
        .data_service
        .unlock_user(uid)
        .await
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "Failed to unlock user"))?;

    Ok(StatusCode::OK)
}

/// Get server settings.
///
/// When no row has ever been written, this reports the numeric defaults
/// alongside the chain ids the server is actually gating on right now - not
/// `ServerSettings::default()`'s hardcoded mainnet list, which this process
/// may not be enforcing at all. See `unconfigured_chain_ids`.
#[utoipa::path(
    get,
    path = "/admin/settings",
    tag = "admin",
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "Server settings", body = ServerSettingsResponse),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Admin access required"),
    )
)]
pub async fn get_settings<A>(
    AdminAuth(_admin): AdminAuth,
    State(state): State<PgAppState<A>>,
) -> Result<Json<ServerSettingsResponse>, StatusCode>
where
    A: SessionService + 'static,
{
    let row = state
        .data_service
        .get_server_settings()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // No row means `chain_has_no_adapter` is not using `enabled_chain_ids` at
    // all - it falls back to `unconfigured_chain_ids()` instead (see there).
    // Reporting `ServerSettings::default()`'s hardcoded mainnet list here was
    // the bug: it showed chains the server was not actually gating on, and
    // saving anything else on the page wrote that list verbatim, which does
    // not contain Sepolia. Reporting the same set the server is actually
    // using makes an unrelated save a no-op instead of a change.
    let enabled_chain_ids = match &row {
        Some(settings) => settings.enabled_chain_ids.clone(),
        None => unconfigured_chain_ids(),
    };
    let settings = row.unwrap_or_default();

    // The response fields below are still named after `api_types::admin`'s
    // shared DTO, which is pinned by rev from payserver-commons and has not
    // yet picked up the matching rename - see that repository's own commit.
    Ok(Json(ServerSettingsResponse {
        default_confirmations: settings.default_confirmations,
        invoice_expiry_minutes: settings.invoice_expiry_minutes,
        rate_limit_rpm: settings.rate_limit_rpm,
        enabled_chain_ids,
        operator_store_id: settings.operator_store_id,
        // What this process resolved at boot, compared with what is stored.
        // They differ after a change nobody has restarted into, and an admin
        // needs to be able to tell - otherwise the page shows a store the
        // server is not actually using.
        operator_store_id_active: state.operator_store_id == settings.operator_store_id,
    }))
}

/// The chains a build accepts for a brand-new payment method when no
/// `server_settings` row exists yet, mirroring `chain_has_no_adapter`'s
/// `None` + `New` branch exactly - see that function's doc for why this is
/// `evm::testnet` and not the full registry. Its own function so `get_settings`
/// can report the same set it is actually gated by, rather than a value that
/// looks current but isn't.
fn unconfigured_chain_ids() -> Vec<ChainId> {
    evm::testnet::ALL_TESTNETS
        .iter()
        .map(|config| ChainId::evm(config.chain_id))
        .collect()
}

/// Whether `id` names one of the real mainnet chains, per the one registry
/// that defines "mainnet" (`evm::network::ALL_CHAINS`) rather than a second,
/// separately-maintained list that can drift from it. Non-EVM and testnet
/// ids both answer `false` here - a Tron or Sepolia id is not what
/// `mainnet_enabled` gates.
fn is_mainnet_chain_id(id: &ChainId) -> bool {
    id.evm_chain_id()
        .is_some_and(|cid| evm::network::get_chain_config_by_id(cid).is_some())
}

/// A mainnet chain id reaching `enabled_chain_ids` is a decision to accept
/// real value on this server - deploy config, not an admin API call. The
/// switch this checks (`Config::mainnet_enabled`) is read once at process
/// start and has no setter anywhere in this API, including here: an admin
/// with nothing but a session token can never flip it themselves. Both
/// outcomes are logged - the refusal and the accepted write - with the chain
/// ids and the caller, since this is exactly the kind of change that must
/// leave a record whichever way it goes.
fn guard_mainnet_chain_ids(
    admin_id: &UserId,
    enabled_chain_ids: &[ChainId],
    mainnet_enabled: bool,
) -> Result<(), StatusCode> {
    let requested_mainnet_ids: Vec<&ChainId> = enabled_chain_ids
        .iter()
        .filter(|id| is_mainnet_chain_id(id))
        .collect();
    if requested_mainnet_ids.is_empty() {
        return Ok(());
    }
    if !mainnet_enabled {
        tracing::error!(
            actor = %admin_id,
            chain_ids = ?requested_mainnet_ids,
            "refused: mainnet chain id in enabled_chain_ids with MAINNET_ENABLED unset"
        );
        return Err(StatusCode::FORBIDDEN);
    }
    tracing::error!(
        actor = %admin_id,
        chain_ids = ?requested_mainnet_ids,
        "mainnet chain id written to enabled_chain_ids"
    );
    Ok(())
}

/// Update server settings (upsert).
#[utoipa::path(
    put,
    path = "/admin/settings",
    tag = "admin",
    security(("bearer_auth" = [])),
    request_body = UpdateServerSettingsRequest,
    responses(
        (status = 200, description = "Settings updated"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Admin access required"),
    )
)]
pub async fn update_settings<A>(
    AdminAuth(admin): AdminAuth,
    State(state): State<PgAppState<A>>,
    Json(body): Json<UpdateServerSettingsRequest>,
) -> Result<StatusCode, StatusCode>
where
    A: SessionService + 'static,
{
    let row = state
        .data_service
        .get_server_settings()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // Same substitution `get_settings` already makes on its read path, for
    // the same reason: `ServerSettings::default()` reports the hardcoded
    // mainnet list, which is not the set a row-less instance actually gates
    // on (`chain_has_no_adapter` falls back to `unconfigured_chain_ids()`
    // there too). Without this, an unrelated first save on a fresh instance
    // - one that never mentions chains at all - would persist that mainnet
    // list into `enabled_chain_ids` verbatim, which the mainnet guard below
    // would then correctly refuse: the right failure, but for a save that
    // had nothing to do with chains.
    let current_chain_ids = match &row {
        Some(settings) => settings.enabled_chain_ids.clone(),
        None => unconfigured_chain_ids(),
    };
    let current = ServerSettings {
        enabled_chain_ids: current_chain_ids,
        ..row.unwrap_or_default()
    };

    // Absent leaves it alone; `Some(None)` clears it. A plain `Option` could
    // not tell those apart, and every client that saves the other four
    // settings without knowing about this field would clear it.
    let operator_store_id = match body.operator_store_id {
        Some(next) => next,
        None => current.operator_store_id,
    };

    if let Some(store_id) = operator_store_id
        && operator_store_id != current.operator_store_id
    {
        validate_operator_store(&state, store_id).await?;
        // `error`, not `info`: this field is `Config::operator_store_id`, and
        // that doc comment already explains why no value here is safely
        // wrong. A change to it must reach Sentry as an event - which is
        // what turns into a ticket - and not sit as a log line nobody was
        // looking at. Only reachable on an actual value change, same as the
        // validation above, so re-saving an unchanged settings form stays
        // silent.
        tracing::error!(
            actor = %admin.id,
            store_id = %store_id,
            "operator store changed; it takes effect on the next restart"
        );
    }

    // Absent leaves the stored list alone. A stored `enabled_chain_ids` is
    // authoritative - every chain not in it is refused - and the GET above
    // answers with compiled-in mainnet defaults when no row exists, so a
    // client round-tripping what it was handed would write a list nobody
    // chose. On a testnet deployment that list has no Sepolia in it.
    let enabled_chain_ids = body.enabled_chain_ids.unwrap_or(current.enabled_chain_ids);

    guard_mainnet_chain_ids(&admin.id, &enabled_chain_ids, state.mainnet_enabled)?;

    let settings = ServerSettings {
        default_confirmations: body.default_confirmations,
        invoice_expiry_minutes: body.invoice_expiry_minutes,
        rate_limit_rpm: body.rate_limit_rpm,
        enabled_chain_ids,
        operator_store_id,
    };

    state
        .data_service
        .upsert_server_settings(&settings)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(StatusCode::OK)
}

/// Refuse an operator store that is not the operator's own, or that could not
/// actually be invoiced on.
///
/// Checked here rather than at boot because here there is a human to tell.
/// Every one of these failures is silent otherwise: the setting saves, the
/// server restarts, and the first sign of trouble is a merchant clicking Pay
/// and getting nothing - by which point nobody connects it to a settings
/// change made days earlier. Ownership and invoiceability are independent
/// properties of the nominated store, checked one after the other, and
/// neither stands in for the other.
///
/// Not a foreign key, for the reason the migration gives: a settings row must
/// not be what stops a store being deleted.
async fn validate_operator_store<A>(
    state: &PgAppState<A>,
    store_id: types::StoreId,
) -> Result<(), StatusCode>
where
    A: SessionService + 'static,
{
    let store = auth::StoreRepository::get_store(&*state.data_service, store_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    if let Err(reason) = owned_by_operator(store.as_ref(), state.operator_account_id) {
        tracing::warn!(%store_id, reason, "refused an operator store nomination");
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }

    let methods = data_service::StorePaymentMethodReader::get_enabled_payment_methods(
        &*state.data_service,
        store_id.0,
    )
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    if let Err(reason) = billable(&methods) {
        tracing::warn!(%store_id, reason, "refused an operator store that cannot be invoiced on");
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }

    Ok(())
}

/// Who may be nominated as the store this instance issues its own invoices
/// through.
///
/// Its own function so the rule is testable without a database, the same
/// reason `billable` below is split out. `operator_account_id` absent refuses
/// every store rather than none of them: a missing operator account and one
/// an attacker cleared are indistinguishable from inside this function, so
/// both have to fail closed.
fn owned_by_operator(
    store: Option<&types::Store>,
    operator_account_id: Option<types::UserId>,
) -> Result<(), &'static str> {
    let Some(store) = store else {
        return Err("the store does not exist");
    };
    match operator_account_id {
        Some(id) if store.owner_id == id => Ok(()),
        Some(_) => Err("the store does not belong to the operator's designated account"),
        None => Err("no operator account is configured"),
    }
}

/// Whether an invoice could actually be issued and paid on these methods.
///
/// Its own function so the rule is testable without a database. Both
/// failures produce the same refusal but not the same log line - an operator
/// who enabled no method and one whose wallet does not resolve have
/// different things to go and fix.
fn billable(methods: &[data_service::StorePaymentMethod]) -> Result<(), &'static str> {
    if methods.is_empty() {
        return Err("the store has no enabled payment method");
    }
    // `get_enabled_payment_methods` resolves each method's wallet through the
    // pin -> store override -> account primary walk, so this reads the
    // *resolved* answer and not the raw column. A store where nothing
    // resolves can quote no address, so an invoice issued on it could never
    // be paid.
    if methods.iter().all(|m| m.wallet_id.is_none()) {
        return Err("no enabled payment method resolves to a wallet");
    }
    Ok(())
}

/// Whether this boot has every plugin disabled.
#[utoipa::path(
    get,
    path = "/admin/safe-mode",
    tag = "admin",
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "Safe mode status", body = SafeModeResponse),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Admin access required"),
    )
)]
pub async fn get_safe_mode<A>(
    AdminAuth(_admin): AdminAuth,
    State(state): State<PgAppState<A>>,
) -> Json<SafeModeResponse>
where
    A: SessionService + 'static,
{
    Json(safe_mode_response(&*state.data_service, state.safe_mode).await)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use data_service::test_support::pg_service;
    use types::ChainId;

    #[test]
    fn test_user_list_response_serialization() {
        let resp = UserListResponse {
            users: vec![AdminUserInfo {
                id: "abc-123".to_string(),
                email: Some("test@example.com".to_string()),
                primary_wallet_address: None,
                role: "server_admin".to_string(),
                created_at: Utc::now(),
                last_login_at: None,
                locked_until: None,
            }],
            total: 1,
            offset: 0,
            limit: 50,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["total"], 1);
        assert_eq!(json["users"][0]["role"], "server_admin");
    }

    #[test]
    fn test_server_settings_response_serialization() {
        let resp = ServerSettingsResponse {
            default_confirmations: 3,
            invoice_expiry_minutes: 60,
            rate_limit_rpm: 100,
            enabled_chain_ids: vec![ChainId::evm(1), ChainId::evm(137)],
            operator_store_id: None,
            operator_store_id_active: true,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["default_confirmations"], 3);
        assert_eq!(
            json["enabled_chain_ids"],
            serde_json::json!(["eip155:1", "eip155:137"])
        );
    }

    #[test]
    fn an_unconfigured_instance_reports_sepolia_and_not_the_mainnet_default() {
        let chain_ids = unconfigured_chain_ids();
        assert!(chain_ids.contains(&ChainId::evm(11_155_111)));
        // The bug this guards: an unconfigured deployment must never report
        // the mainnet default list, because a testnet instance actually
        // gates on this set - not that one - and saving it verbatim would
        // start refusing Sepolia.
        assert_ne!(chain_ids, ServerSettings::default().enabled_chain_ids);
    }

    /// The reported set and the gated set must be the same set.
    ///
    /// Two functions that have to agree is how this repository keeps getting
    /// caught, and "it mirrors the other one" is a claim that holds until
    /// someone edits one of them. Today it cannot drift - `get_testnet_config`
    /// is a linear search of `ALL_TESTNETS`, so both read the same slice - but
    /// reimplementing it as a match would break the mirror silently, and the
    /// symptom would be a settings page offering chains the server refuses.
    ///
    /// So: ask the gate itself, chain by chain, and require it to accept
    /// exactly what this function reports.
    #[test]
    fn every_chain_reported_when_unconfigured_is_one_the_gate_actually_accepts() {
        use crate::api::stores::{ChainCheckContext, chain_has_no_adapter};

        for chain_id in unconfigured_chain_ids() {
            assert!(
                !chain_has_no_adapter(&chain_id, None, ChainCheckContext::New),
                "{chain_id} is offered by the settings page and refused by the gate"
            );
        }

        // And the other direction, on the one that matters: a chain the gate
        // refuses must not be offered. Mainnet is the case with teeth, since
        // it is what `ServerSettings::default()` used to report here.
        let mainnet = ChainId::evm(1);
        assert!(
            chain_has_no_adapter(&mainnet, None, ChainCheckContext::New),
            "an unconfigured instance does not accept mainnet for a new method"
        );
        assert!(
            !unconfigured_chain_ids().contains(&mainnet),
            "so the settings page must not offer it"
        );
    }

    /// `is_mainnet_chain_id` is correct only as long as `ALL_CHAINS` holds
    /// nothing but mainnets - it does not special-case testnet ids, it just
    /// trusts that list. If a testnet const were ever added there, this
    /// predicate would silently invert: it would call Sepolia "mainnet" and
    /// the guard would start refusing testnet operation unless
    /// `MAINNET_ENABLED` were set, pressuring whoever runs testnet into
    /// flipping the one switch this feature exists to gate. This pins the
    /// assumption directly, so that inversion fails here first.
    #[test]
    fn testnet_chain_ids_are_not_mainnet() {
        for (chain_id, name) in [
            (11_155_111, "Sepolia"),
            (17_000, "Holesky"),
            (80_002, "Polygon Amoy"),
        ] {
            assert!(
                !is_mainnet_chain_id(&ChainId::evm(chain_id)),
                "{name} ({chain_id}) must not be classified as mainnet"
            );
        }
    }

    #[test]
    fn test_safe_mode_response_serialization() {
        let resp = SafeModeResponse {
            safe_mode: true,
            operator_store_available: false,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["safe_mode"], true);
        assert_eq!(json["operator_store_available"], false);
    }

    /// The operator store is offered only when a plugin could use it.
    #[test]
    fn the_operator_store_is_available_only_with_an_enabled_plugin() {
        assert!(!operator_store_available(false, []), "no plugins installed");
        assert!(
            !operator_store_available(false, [false]),
            "the only plugin is disabled"
        );
        assert!(operator_store_available(false, [false, true]));
        assert!(
            !operator_store_available(true, [true]),
            "safe mode loads nothing"
        );
    }

    struct Plugins(Result<Vec<bool>, ()>);

    #[async_trait::async_trait]
    impl data_service::InstalledPluginReader for Plugins {
        async fn list_installed_plugins(
            &self,
        ) -> data_service::RepositoryResult<Vec<data_service::InstalledPlugin>> {
            let Ok(flags) = &self.0 else {
                return Err(data_service::RepositoryError::InvalidData("down".into()));
            };
            Ok(flags
                .iter()
                .map(|&enabled| data_service::InstalledPlugin {
                    id: "p".to_string(),
                    version: "1.0.0".to_string(),
                    manifest_toml: String::new(),
                    artifact_sha256: String::new(),
                    enabled,
                    disabled_reason: None,
                    db_role_password: None,
                    installed_at: Utc::now(),
                    updated_at: Utc::now(),
                })
                .collect())
        }
        async fn get_installed_plugin(
            &self,
            _id: &str,
        ) -> data_service::RepositoryResult<Option<data_service::InstalledPlugin>> {
            Ok(None)
        }
        async fn plugin_events(
            &self,
            _id: &str,
            _limit: i64,
        ) -> data_service::RepositoryResult<Vec<data_service::PluginEvent>> {
            Ok(Vec::new())
        }
    }

    async fn response_json(plugins: Result<Vec<bool>, ()>, safe_mode: bool) -> serde_json::Value {
        serde_json::to_value(safe_mode_response(&Plugins(plugins), safe_mode).await).unwrap()
    }

    /// The handler's body, end to end over the wire shape: it must read the
    /// installed rows, and a failed read must not take `safe_mode` with it.
    #[tokio::test]
    async fn the_safe_mode_response_reflects_the_installed_plugins() {
        let none = response_json(Ok(vec![]), false).await;
        assert_eq!(none["operator_store_available"], false);
        assert_eq!(none["safe_mode"], false);

        let off = response_json(Ok(vec![false]), false).await;
        assert_eq!(off["operator_store_available"], false);

        let on = response_json(Ok(vec![false, true]), false).await;
        assert_eq!(on["operator_store_available"], true);

        let safe = response_json(Ok(vec![true]), true).await;
        assert_eq!(safe["safe_mode"], true);
        assert_eq!(safe["operator_store_available"], false);

        let failed = response_json(Err(()), true).await;
        assert_eq!(failed["safe_mode"], true, "safe_mode survives a read error");
        assert_eq!(failed["operator_store_available"], false);

        let failed_live = response_json(Err(()), false).await;
        assert_eq!(failed_live["safe_mode"], false);
        assert_eq!(
            failed_live["operator_store_available"], false,
            "an unreadable plugin list reports the store unavailable"
        );
    }

    fn method(wallet: Option<uuid::Uuid>) -> data_service::StorePaymentMethod {
        data_service::StorePaymentMethod {
            id: uuid::Uuid::new_v4(),
            store_id: uuid::Uuid::new_v4(),
            chain_id: ChainId::evm(11155111),
            token_address: None,
            asset_symbol: "USDC".to_string(),
            decimals: 6,
            wallet_id: wallet,
            xpub: wallet.map(|_| "xpub".to_string()),
            derivation_index: wallet.map(|_| 0),
            enabled: true,
            created_at: chrono::Utc::now(),
        }
    }

    /// An operator store is checked when it is set, because that is the only
    /// moment a human is present. Every one of these failures is otherwise
    /// silent until a merchant clicks Pay and gets nothing, days later.
    #[test]
    fn a_store_that_cannot_be_invoiced_on_is_refused() {
        assert_eq!(
            billable(&[]),
            Err("the store has no enabled payment method"),
            "a store with nothing enabled can quote no asset"
        );

        assert_eq!(
            billable(&[method(None)]),
            Err("no enabled payment method resolves to a wallet"),
            "a method with no resolved wallet can quote no address, so its \
             invoice could never be paid"
        );

        assert!(billable(&[method(Some(uuid::Uuid::new_v4()))]).is_ok());

        // One resolving method is enough - the invoice can be paid on that
        // one, and refusing the whole store because a second is unconfigured
        // would block a working setup.
        assert!(billable(&[method(None), method(Some(uuid::Uuid::new_v4()))]).is_ok());
    }

    /// Ownership is its own property, independent of whether the store can
    /// be invoiced on.
    #[test]
    fn a_store_not_owned_by_the_operator_is_refused() {
        let operator = UserId::new();
        let someone_else = types::Store::new("someone else's store", UserId::new());

        assert_eq!(
            owned_by_operator(Some(&someone_else), Some(operator)),
            Err("the store does not belong to the operator's designated account")
        );
    }

    /// The other direction: a check that refuses everything is not a fix.
    #[test]
    fn the_operators_own_store_is_accepted() {
        let operator = UserId::new();
        let own_store = types::Store::new("the operator's own store", operator);

        assert!(owned_by_operator(Some(&own_store), Some(operator)).is_ok());
    }

    /// No configured operator account must refuse every nomination, not let
    /// every store through - the same "no value is safely wrong" reasoning
    /// as `Config::operator_store_id` itself.
    #[test]
    fn no_operator_account_configured_refuses_every_store() {
        let any_store = types::Store::new("any store", UserId::new());

        assert_eq!(
            owned_by_operator(Some(&any_store), None),
            Err("no operator account is configured")
        );
    }

    #[test]
    fn a_nonexistent_store_is_refused() {
        assert_eq!(
            owned_by_operator(None, Some(UserId::new())),
            Err("the store does not exist")
        );
    }

    // ========================================================================
    // `update_settings` against a real database.
    //
    // `#[ignore]`d, and fails naming `DATABASE_URL` when it is unset or the
    // database is unreachable, the same convention as every other
    // database-backed test in this codebase (see
    // `server/src/api/stores/tests.rs`'s handler tests, or
    // `data-service/src/postgres/integration_tests/*`).
    // ========================================================================

    /// Exists only to give `PgAppState<A>` a concrete auth-service type;
    /// never called because `AdminAuth` below is constructed directly.
    struct NoAuthSessionService;

    #[async_trait::async_trait]
    impl SessionService for NoAuthSessionService {
        async fn validate_session(
            &self,
            _session_id: auth::SessionId,
        ) -> auth::Result<(auth::UserInfo, auth::Session)> {
            Err(auth::AuthError::InvalidCredentials)
        }

        async fn logout(&self, _session_id: auth::SessionId) -> auth::Result<()> {
            Err(auth::AuthError::InvalidCredentials)
        }

        async fn logout_all(&self, _session_id: auth::SessionId) -> auth::Result<()> {
            Err(auth::AuthError::InvalidCredentials)
        }

        async fn cleanup_stale_sessions(&self) -> auth::Result<u64> {
            Err(auth::AuthError::InvalidCredentials)
        }
    }

    async fn settings_test_user(pool: &sqlx::PgPool) -> uuid::Uuid {
        let user_id = uuid::Uuid::new_v4();
        sqlx::query(
            "INSERT INTO users (id, kdf_params, encrypted_symmetric_key, \
             recovery_verification_hash, kdf_salt_identifier) \
             VALUES ($1, '{}'::jsonb, '{}'::jsonb, 'h', 'passkey:' || $1::text)",
        )
        .bind(user_id)
        .execute(pool)
        .await
        .expect("seed user");
        user_id
    }

    async fn settings_test_store(pool: &sqlx::PgPool, owner: uuid::Uuid) -> uuid::Uuid {
        let store_id = uuid::Uuid::new_v4();
        sqlx::query("INSERT INTO stores (id, name, owner_id) VALUES ($1, $2, $3)")
            .bind(store_id)
            .bind(format!("store-{store_id}"))
            .bind(owner)
            .execute(pool)
            .await
            .expect("seed store");
        store_id
    }

    fn settings_admin(user_id: uuid::Uuid) -> AdminAuth {
        AdminAuth(auth::UserInfo {
            id: UserId(user_id),
            email: None,
            primary_wallet_address: None,
            created_at: Utc::now(),
            last_login_at: None,
            role: Role::ServerAdmin,
        })
    }

    fn settings_test_state(
        service: data_service::PgDataService,
        operator_account_id: Option<UserId>,
    ) -> PgAppState<NoAuthSessionService> {
        let mut state = PgAppState::new(
            std::sync::Arc::new(service),
            std::sync::Arc::new(NoAuthSessionService),
            None,
            std::sync::Arc::new(rates::NoOpRateProvider),
            std::sync::Arc::new(crate::services::email::NoopEmailSender),
        );
        state.operator_account_id = operator_account_id;
        state
    }

    fn settings_body(
        operator_store_id: Option<Option<types::StoreId>>,
    ) -> UpdateServerSettingsRequest {
        UpdateServerSettingsRequest {
            default_confirmations: 3,
            invoice_expiry_minutes: 60,
            rate_limit_rpm: 100,
            enabled_chain_ids: None,
            operator_store_id,
        }
    }

    /// Matches on the event's own message, not just its level - an
    /// unrelated `error!` elsewhere in the call path must not make this
    /// pass for the wrong reason.
    struct CapturesError(std::sync::Arc<std::sync::atomic::AtomicBool>);

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CapturesError {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            if *event.metadata().level() != tracing::Level::ERROR {
                return;
            }
            struct FindsOperatorStoreChanged(bool);
            impl tracing::field::Visit for FindsOperatorStoreChanged {
                fn record_debug(
                    &mut self,
                    field: &tracing::field::Field,
                    value: &dyn std::fmt::Debug,
                ) {
                    if field.name() == "message"
                        && format!("{value:?}").contains("operator store changed")
                    {
                        self.0 = true;
                    }
                }
            }
            let mut visitor = FindsOperatorStoreChanged(false);
            event.record(&mut visitor);
            if visitor.0 {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
    }

    /// The alarm (`tracing::error!`, which Sentry turns into an event and
    /// this deployment's pipeline turns into a ticket) must fire exactly when
    /// the operator store actually changes, not on every settings save - or
    /// it becomes noise nobody reads. The `Some(None)` versus absent
    /// distinction on `UpdateServerSettingsRequest::operator_store_id` is what
    /// makes a resave of the same value distinguishable from a real change
    /// at all.
    #[tokio::test]
    #[ignore]
    async fn the_operator_store_alarm_fires_only_on_an_actual_change() {
        use tracing_subscriber::prelude::*;

        let service = pg_service().await;
        let pool = service.pool().clone();
        sqlx::query("DELETE FROM server_settings WHERE id = 1")
            .execute(&pool)
            .await
            .expect("reset settings");

        let owner = settings_test_user(&pool).await;
        let store_id = settings_test_store(&pool, owner).await;
        let xpub = format!("xpub-test-{}", uuid::Uuid::new_v4());
        data_service::StorePaymentMethodWriter::create_payment_method(
            &service,
            store_id,
            &ChainId::evm(11155111),
            None,
            "ETH",
            18,
            Some(&xpub),
        )
        .await
        .expect("seed a resolving payment method");

        let state = settings_test_state(service, Some(UserId(owner)));

        let saw_error = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let _dispatcher = tracing_subscriber::registry()
            .with(CapturesError(std::sync::Arc::clone(&saw_error)))
            .set_default();

        let status = update_settings(
            settings_admin(owner),
            State(state.clone()),
            Json(settings_body(Some(Some(types::StoreId(store_id))))),
        )
        .await
        .expect("the operator's own, billable store must be accepted");
        assert_eq!(status, StatusCode::OK);
        assert!(
            saw_error.load(std::sync::atomic::Ordering::SeqCst),
            "an actual operator store change must raise the alarm"
        );

        saw_error.store(false, std::sync::atomic::Ordering::SeqCst);

        let status = update_settings(
            settings_admin(owner),
            State(state.clone()),
            Json(settings_body(Some(Some(types::StoreId(store_id))))),
        )
        .await
        .expect("resaving the same store must still succeed");
        assert_eq!(status, StatusCode::OK);
        assert!(
            !saw_error.load(std::sync::atomic::Ordering::SeqCst),
            "resaving the same value must not raise the alarm again - it would stop being read"
        );

        sqlx::query("DELETE FROM server_settings WHERE id = 1")
            .execute(&pool)
            .await
            .expect("clean up settings");
    }

    /// The negative half of the same property this file's other tests cover
    /// in isolation (`owned_by_operator`) - here proven through the real
    /// endpoint, against a real database, the same way
    /// `server/src/api/stores/tests.rs` proves its chain gate through
    /// `create_payment_method` rather than trusting the pure predicate alone.
    #[tokio::test]
    #[ignore]
    async fn the_endpoint_refuses_a_store_the_operator_does_not_own() {
        let service = pg_service().await;
        let pool = service.pool().clone();
        sqlx::query("DELETE FROM server_settings WHERE id = 1")
            .execute(&pool)
            .await
            .expect("reset settings");

        let operator = settings_test_user(&pool).await;
        let merchant = settings_test_user(&pool).await;
        let merchants_store = settings_test_store(&pool, merchant).await;
        let xpub = format!("xpub-test-{}", uuid::Uuid::new_v4());
        data_service::StorePaymentMethodWriter::create_payment_method(
            &service,
            merchants_store,
            &ChainId::evm(11155111),
            None,
            "ETH",
            18,
            Some(&xpub),
        )
        .await
        .expect("seed a resolving payment method - fully billable, not the failure under test");

        let state = settings_test_state(service, Some(UserId(operator)));

        let status = update_settings(
            settings_admin(operator),
            State(state),
            Json(settings_body(Some(Some(types::StoreId(merchants_store))))),
        )
        .await
        .expect_err("a billable store owned by someone else must still be refused");
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

        sqlx::query("DELETE FROM server_settings WHERE id = 1")
            .execute(&pool)
            .await
            .expect("clean up settings");
    }

    /// Ownership does not stand in for billability - the check this file adds
    /// runs ahead of the existing one, not instead of it. A store the
    /// operator owns but that resolves to no wallet must still be refused,
    /// the same property `a_store_that_cannot_be_invoiced_on_is_refused`
    /// proves in isolation, here proven through the real endpoint.
    #[tokio::test]
    #[ignore]
    async fn an_owned_store_that_cannot_be_invoiced_on_is_still_refused() {
        let service = pg_service().await;
        let pool = service.pool().clone();
        sqlx::query("DELETE FROM server_settings WHERE id = 1")
            .execute(&pool)
            .await
            .expect("reset settings");

        let operator = settings_test_user(&pool).await;
        let operators_store = settings_test_store(&pool, operator).await;

        let state = settings_test_state(service, Some(UserId(operator)));

        let status = update_settings(
            settings_admin(operator),
            State(state),
            Json(settings_body(Some(Some(types::StoreId(operators_store))))),
        )
        .await
        .expect_err("an owned store with no usable payment method must still be refused");
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

        sqlx::query("DELETE FROM server_settings WHERE id = 1")
            .execute(&pool)
            .await
            .expect("clean up settings");
    }

    fn chain_ids_body(enabled_chain_ids: Vec<ChainId>) -> UpdateServerSettingsRequest {
        UpdateServerSettingsRequest {
            default_confirmations: 3,
            invoice_expiry_minutes: 60,
            rate_limit_rpm: 100,
            enabled_chain_ids: Some(enabled_chain_ids),
            operator_store_id: None,
        }
    }

    /// Matches on message substring, same pattern as `CapturesError` above -
    /// generalized to the caller's text instead of one hardcoded message,
    /// since this file now has two different `error!` calls worth asserting on.
    struct CapturesMessage(std::sync::Arc<std::sync::atomic::AtomicBool>, &'static str);

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CapturesMessage {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            if *event.metadata().level() != tracing::Level::ERROR {
                return;
            }
            struct FindsText(bool, &'static str);
            impl tracing::field::Visit for FindsText {
                fn record_debug(
                    &mut self,
                    field: &tracing::field::Field,
                    value: &dyn std::fmt::Debug,
                ) {
                    if field.name() == "message" && format!("{value:?}").contains(self.1) {
                        self.0 = true;
                    }
                }
            }
            let mut visitor = FindsText(false, self.1);
            event.record(&mut visitor);
            if visitor.0 {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
    }

    /// The property the `MAINNET_ENABLED` switch exists for: an admin with
    /// nothing but a session token cannot write a mainnet chain id into
    /// `enabled_chain_ids` by calling the API, and the refusal is logged with
    /// the chain id and the caller - not a silent 403.
    #[tokio::test]
    #[ignore]
    async fn the_endpoint_refuses_a_mainnet_chain_id_when_mainnet_is_not_enabled() {
        let service = pg_service().await;
        let pool = service.pool().clone();
        sqlx::query("DELETE FROM server_settings WHERE id = 1")
            .execute(&pool)
            .await
            .expect("reset settings");

        let operator = settings_test_user(&pool).await;
        let state = settings_test_state(service, Some(UserId(operator)));
        assert!(
            !state.mainnet_enabled,
            "off by default, same as Config's own default"
        );

        let saw_refusal = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        use tracing_subscriber::prelude::*;
        let _dispatcher = tracing_subscriber::registry()
            .with(CapturesMessage(
                std::sync::Arc::clone(&saw_refusal),
                "mainnet chain id in enabled_chain_ids with MAINNET_ENABLED unset",
            ))
            .set_default();

        let status = update_settings(
            settings_admin(operator),
            State(state),
            Json(chain_ids_body(vec![ChainId::evm(1)])),
        )
        .await
        .expect_err("a mainnet chain id must be refused when the process-level switch is off");
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(
            saw_refusal.load(std::sync::atomic::Ordering::SeqCst),
            "the refusal must be logged"
        );

        sqlx::query("DELETE FROM server_settings WHERE id = 1")
            .execute(&pool)
            .await
            .expect("clean up settings");
    }

    /// The other half: with the deploy-time switch on, the same write
    /// succeeds and is still logged - an accepted mainnet enablement must
    /// leave a record exactly as much as a refused one does.
    #[tokio::test]
    #[ignore]
    async fn the_endpoint_accepts_a_mainnet_chain_id_when_mainnet_is_enabled() {
        let service = pg_service().await;
        let pool = service.pool().clone();
        sqlx::query("DELETE FROM server_settings WHERE id = 1")
            .execute(&pool)
            .await
            .expect("reset settings");

        let operator = settings_test_user(&pool).await;
        let mut state = settings_test_state(service, Some(UserId(operator)));
        state.mainnet_enabled = true;

        let saw_acceptance = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        use tracing_subscriber::prelude::*;
        let _dispatcher = tracing_subscriber::registry()
            .with(CapturesMessage(
                std::sync::Arc::clone(&saw_acceptance),
                "mainnet chain id written to enabled_chain_ids",
            ))
            .set_default();

        let status = update_settings(
            settings_admin(operator),
            State(state),
            Json(chain_ids_body(vec![ChainId::evm(1)])),
        )
        .await
        .expect("a mainnet chain id must be accepted when the process-level switch is on");
        assert_eq!(status, StatusCode::OK);
        assert!(
            saw_acceptance.load(std::sync::atomic::Ordering::SeqCst),
            "the acceptance must be logged too"
        );

        sqlx::query("DELETE FROM server_settings WHERE id = 1")
            .execute(&pool)
            .await
            .expect("clean up settings");
    }

    /// The bug the write path had and the read path (`get_settings`) already
    /// fixed: a fresh instance with no `server_settings` row must not have an
    /// UNRELATED save (one that never mentions chains) persist
    /// `ServerSettings::default()`'s hardcoded mainnet list into
    /// `enabled_chain_ids`. That list would then also trip the mainnet guard
    /// above on every later save, for a value nobody chose.
    #[tokio::test]
    #[ignore]
    async fn an_unrelated_save_on_a_fresh_instance_does_not_persist_mainnet_defaults() {
        let service = pg_service().await;
        let pool = service.pool().clone();
        sqlx::query("DELETE FROM server_settings WHERE id = 1")
            .execute(&pool)
            .await
            .expect("reset settings");

        let operator = settings_test_user(&pool).await;
        let state = settings_test_state(service, Some(UserId(operator)));

        let status = update_settings(
            settings_admin(operator),
            State(state.clone()),
            Json(settings_body(None)),
        )
        .await
        .expect("an unrelated save must succeed on a fresh instance");
        assert_eq!(status, StatusCode::OK);

        // Through the repository, not a raw query: `enabled_chain_ids` is a
        // `caip2[]` domain column, and sqlx decodes arrays by element type
        // OID - asking for it as `serde_json::Value` or `Vec<String>` fails
        // the decode regardless of the values, the exact defect
        // data-service's own server_settings integration tests exist to
        // catch. The repository already knows how to read its own column.
        let saved = state
            .data_service
            .get_server_settings()
            .await
            .expect("read back settings")
            .expect("settings row must exist after the save")
            .enabled_chain_ids;
        let expected = unconfigured_chain_ids();
        assert_eq!(
            saved, expected,
            "must persist the unconfigured (testnet) set, not ServerSettings::default()'s mainnet list"
        );
        assert!(
            !expected.contains(&ChainId::evm(1)),
            "sanity: the set we just asserted equals must not itself be the mainnet default"
        );

        sqlx::query("DELETE FROM server_settings WHERE id = 1")
            .execute(&pool)
            .await
            .expect("clean up settings");
    }
}
