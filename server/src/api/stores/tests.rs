#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use auth::{ServerSettings, Store, StoreId, UserId};
use chrono::Utc;
use data_service::StorePaymentMethod;
use data_service::test_support::pg_service;
use types::ChainId;
use uuid::Uuid;

// =========================================================================
// mask_xpub
// =========================================================================

#[test]
fn test_mask_xpub_normal() {
    let xpub = "xpub6DCoCpSuQZB2jawqnGMEPS63ePKWkwWPH4TU45Q7LPXWuNd8TMtVxRrgjtEshuqpK3mdhaWHPFsBngh5GFZaM6si3yZdUsT8ddYM3PwnATt";
    let masked = mask_xpub(xpub);
    assert!(masked.starts_with("xpub6DCo"));
    assert!(masked.contains("..."));
    assert!(masked.ends_with("PwnATt"));
}

#[test]
fn test_mask_xpub_short() {
    assert_eq!(mask_xpub("short"), "****");
}

#[test]
fn test_mask_xpub_boundary_20() {
    assert_eq!(mask_xpub("12345678901234567890"), "****");
}

#[test]
fn test_mask_xpub_boundary_21() {
    assert_eq!(mask_xpub("123456789012345678901"), "12345678...45678901");
}

// =========================================================================
// StoreResponse conversion
// =========================================================================

#[test]
fn test_store_response_from_store() {
    let now = Utc::now();
    let store = Store {
        id: StoreId(Uuid::nil()),
        name: "Test Store".to_string(),
        website: Some("https://example.com".to_string()),
        owner_id: UserId(Uuid::nil()),
        archived: false,
        created_at: now,
    };

    let response = store_response(store);
    assert_eq!(response.id, Uuid::nil());
    assert_eq!(response.name, "Test Store");
    assert_eq!(response.website, Some("https://example.com".to_string()));
    assert_eq!(response.owner_id, Uuid::nil());
    assert!(!response.archived);
    assert_eq!(response.created_at, now);
}

#[test]
fn test_store_response_without_website() {
    let store = Store {
        id: StoreId(Uuid::new_v4()),
        name: "No Website".to_string(),
        website: None,
        owner_id: UserId(Uuid::new_v4()),
        archived: false,
        created_at: Utc::now(),
    };

    let response = store_response(store);
    assert!(response.website.is_none());
}

#[test]
fn test_store_response_archived() {
    let store = Store {
        id: StoreId(Uuid::new_v4()),
        name: "Archived".to_string(),
        website: None,
        owner_id: UserId(Uuid::new_v4()),
        archived: true,
        created_at: Utc::now(),
    };

    let response = store_response(store);
    assert!(response.archived);
}

// =========================================================================
// PaymentMethodResponse conversion
// =========================================================================

#[test]
fn test_payment_method_response_native() {
    let pm = StorePaymentMethod {
        id: Uuid::nil(),
        store_id: Uuid::nil(),
        chain_id: ChainId::parse("eip155:1").unwrap(),
        token_address: None,
        asset_symbol: "ETH".to_string(),
        wallet_id: Some(Uuid::new_v4()),
        decimals: 18,
        xpub: Some("xpub6DCoCpSuQZB2jawqnGMEPS63ePKWkwWPH4TU45Q7LPXWuNd8TMtVxRrgjtEshuqpK3mdhaWHPFsBngh5GFZaM6si3yZdUsT8ddYM3PwnATt".to_string()),
        derivation_index: Some(5),
        enabled: true,
        created_at: Utc::now(),
    };

    let response: PaymentMethodResponse = pm.into();
    assert_eq!(response.chain_id, ChainId::parse("eip155:1").unwrap());
    assert_eq!(response.asset_symbol, "ETH");
    assert!(response.token_address.is_none());
    assert_eq!(response.derivation_index, Some(5));
    assert!(response.enabled);
    assert!(response.xpub_masked.unwrap().contains("..."));
}

#[test]
fn test_payment_method_response_erc20() {
    let token_addr = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48".to_string();
    let pm = StorePaymentMethod {
        id: Uuid::nil(),
        store_id: Uuid::nil(),
        chain_id: ChainId::parse("eip155:137").unwrap(),
        token_address: Some(token_addr.clone()),
        asset_symbol: "USDC".to_string(),
        wallet_id: Some(Uuid::new_v4()),
        decimals: 6,
        xpub: Some("xpub6DCoCpSuQZB2jawqnGMEPS63ePKWkwWPH4TU45Q7LPXWuNd8TMtVxRrgjtEshuqpK3mdhaWHPFsBngh5GFZaM6si3yZdUsT8ddYM3PwnATt".to_string()),
        derivation_index: Some(0),
        enabled: false,
        created_at: Utc::now(),
    };

    let response: PaymentMethodResponse = pm.into();
    assert_eq!(response.chain_id, ChainId::parse("eip155:137").unwrap());
    assert_eq!(response.asset_symbol, "USDC");
    assert_eq!(response.token_address, Some(token_addr));
    assert!(!response.enabled);
}

// =========================================================================
// chain_has_no_adapter
// =========================================================================

/// A settings row enabling only mainnet Ethereum and Polygon - no Tron
/// adapter registered, and notably no Sepolia either (see the
/// `an_unconfigured_server_still_accepts_evm` test below for why that
/// matters).
fn evm_only_settings() -> ServerSettings {
    ServerSettings {
        default_confirmations: 3,
        invoice_expiry_minutes: 60,
        rate_limit_rpm: 100,
        enabled_chain_ids: [1u64, 137].into_iter().map(ChainId::evm).collect(),
        operator_store_id: None,
    }
}

/// The hole this ticket closes: a chain with no adapter (here, Tron) must be
/// refused. Without this predicate returning `true` here, `tron:728126428`
/// sails through create/update, gets a `0x...` address from the EVM deriver
/// regardless of namespace, and is never watched - see `derive_payment_address`
/// and `eip155_for_watch`.
#[test]
fn a_chain_with_no_adapter_is_refused() {
    let tron = ChainId::parse("tron:728126428").unwrap();
    assert!(chain_has_no_adapter(
        &tron,
        Some(&evm_only_settings()),
        ChainCheckContext::New
    ));
}

/// The predicate must not also catch a chain the server does serve - a gate
/// that refused everything would pass the test above trivially.
#[test]
fn a_registered_chain_is_not_refused() {
    let settings = ServerSettings {
        enabled_chain_ids: [1u64, 11_155_111].into_iter().map(ChainId::evm).collect(),
        ..evm_only_settings()
    };
    let sepolia = ChainId::parse("eip155:11155111").unwrap();
    assert!(!chain_has_no_adapter(
        &sepolia,
        Some(&settings),
        ChainCheckContext::New
    ));
}

/// This is deliberately not "is it eip155": the predicate is membership in
/// the operator's registered set, not a namespace check. So the day a Tron
/// adapter exists, it is satisfied by the operator adding `tron:...` to
/// `enabled_chain_ids`, not by editing this function - an eip155 chain that
/// was never enabled is refused just the same as Tron is today.
#[test]
fn an_eip155_chain_outside_the_enabled_set_is_still_refused() {
    let untracked = ChainId::parse("eip155:999999").unwrap();
    assert!(chain_has_no_adapter(
        &untracked,
        Some(&evm_only_settings()),
        ChainCheckContext::New
    ));
}

// -------------------------------------------------------------------------
// `settings: None`, `ChainCheckContext::New` - a request introducing a chain
// id (`create`'s `req.chain_id`). Must not guess which environment this
// binary is running as: testnet's one real chain stays accepted, but
// nothing wider does, so an unconfigured mainnet deployment refuses
// everything rather than mistake some other environment's chain for its own.
// -------------------------------------------------------------------------

/// `None` means nobody has ever written a `server_settings` row - true of
/// the live testnet database as of this ticket. Falling back to
/// `ServerSettings::default()` there (a Rust-side, EVM-mainnet chain list)
/// would refuse `eip155:11155111` too, since Sepolia isn't in it - turning
/// this ticket's fix into an outage for the only chain testnet actually
/// serves. `evm::testnet::get_testnet_config` is the fallback used instead:
/// a real testnet chain is accepted when nothing has been explicitly
/// configured yet (see `an_unconfigured_server_refuses_a_mainnet_evm_chain_for_a_new_request`
/// below for why mainnet chains are deliberately excluded from this
/// fallback).
#[test]
fn an_unconfigured_server_still_accepts_evm_for_a_new_request() {
    let sepolia = ChainId::parse("eip155:11155111").unwrap();
    assert!(!chain_has_no_adapter(
        &sepolia,
        None,
        ChainCheckContext::New
    ));
}

/// The unconfigured fallback is EVM-testnet-only, not "accept anything" -
/// Tron must still be refused even before an operator has written a settings
/// row.
#[test]
fn an_unconfigured_server_still_refuses_tron_for_a_new_request() {
    let tron = ChainId::parse("tron:728126428").unwrap();
    assert!(chain_has_no_adapter(&tron, None, ChainCheckContext::New));
}

/// The bug an earlier version of this predicate reopened: falling back to
/// the full compiled chain registry (mainnet and testnet) accepts any
/// mainnet chain id the binary recognizes, not just the testnet chains this
/// (Sepolia-only) deployment actually serves. A merchant on testnet could
/// register `eip155:1` - Ethereum mainnet - get quoted a `0x...` address,
/// and have nothing on the testnet box watching it: the exact hole this
/// ticket exists to close, reopened for any recognized mainnet chain instead
/// of only Tron. Mainnet chains must be refused here; an operator turns one
/// on explicitly via `enabled_chain_ids`, same as Tron would be. Ablated
/// locally (swapped the fallback back to `evm::get_any_chain_config`) and
/// watched this test go red before restoring the testnet-scoped fix.
#[test]
fn an_unconfigured_server_refuses_a_mainnet_evm_chain_for_a_new_request() {
    let ethereum_mainnet = ChainId::parse("eip155:1").unwrap();
    assert!(chain_has_no_adapter(
        &ethereum_mainnet,
        None,
        ChainCheckContext::New
    ));
}

/// The bug in an earlier version of this predicate: falling back to
/// `is_evm()` accepts ANY eip155 number, not just ones this codebase has a
/// config for. `eip155:999999` names no real chain - `evm::testnet::get_testnet_config`
/// returns `None` for it - so it must still be refused even with no settings
/// row, exactly like Tron. Ablated locally (swapped the fallback back to
/// `is_evm()`) and watched this test go red before restoring the fix.
#[test]
fn an_unconfigured_server_refuses_an_unregistered_eip155_id_for_a_new_request() {
    let untracked = ChainId::parse("eip155:999999").unwrap();
    assert!(chain_has_no_adapter(
        &untracked,
        None,
        ChainCheckContext::New
    ));
}

// -------------------------------------------------------------------------
// `settings: None`, `ChainCheckContext::Existing` - a chain already stored
// on a row (`update`'s `existing.chain_id`). Never a value the request
// chose, so there is no environment to guess wrong; the only thing left to
// catch is a namespace with no adapter anywhere (Tron).
// -------------------------------------------------------------------------

/// The bug this context split fixes: with the `New`-style testnet-only
/// fallback applied here too, an unconfigured *mainnet* deployment would
/// 400 on every update to an already-working `eip155:1` row - re-enabling
/// it, rotating its xpub - the moment this ticket shipped, since mainnet
/// chain ids are never in `evm::testnet`'s registry. `Existing` uses the
/// full compiled registry instead, so a stored chain this codebase actually
/// has adapter code for is left alone. Ablated locally (used
/// `ChainCheckContext::New` for this call site too) and watched this test go
/// red before restoring the fix.
#[test]
fn an_unconfigured_server_leaves_an_existing_mainnet_evm_chain_alone() {
    let ethereum_mainnet = ChainId::parse("eip155:1").unwrap();
    assert!(!chain_has_no_adapter(
        &ethereum_mainnet,
        None,
        ChainCheckContext::Existing
    ));
}

/// Same widening, still refuses what has no adapter anywhere - Tron isn't in
/// the compiled registry under any name, so an unconfigured deployment still
/// refuses to leave a legacy Tron row alone.
#[test]
fn an_unconfigured_server_still_refuses_an_existing_tron_chain() {
    let tron = ChainId::parse("tron:728126428").unwrap();
    assert!(chain_has_no_adapter(
        &tron,
        None,
        ChainCheckContext::Existing
    ));
}

/// A testnet chain already stored on a row must still pass under `Existing`:
/// widening the fallback for updates must not narrow it for the chain `New`
/// already accepted.
#[test]
fn an_unconfigured_server_leaves_an_existing_testnet_chain_alone() {
    let sepolia = ChainId::parse("eip155:11155111").unwrap();
    assert!(!chain_has_no_adapter(
        &sepolia,
        None,
        ChainCheckContext::Existing
    ));
}

/// An invented eip155 number is still refused under `Existing` too - the
/// compiled registry doesn't recognize it any more than `evm::testnet` did.
#[test]
fn an_unconfigured_server_refuses_an_unregistered_eip155_id_for_an_existing_chain() {
    let untracked = ChainId::parse("eip155:999999").unwrap();
    assert!(chain_has_no_adapter(
        &untracked,
        None,
        ChainCheckContext::Existing
    ));
}

/// The ticket's example, `tron:728126428`, happens to be Tron's real
/// EVM-compatible chain id - the same number as a genuine `eip155` chain
/// somewhere. `chain_has_no_adapter`'s `None` branch only feeds a namespace's
/// numeric reference to `evm::testnet::get_testnet_config` after `evm_chain_id()`
/// checks `is_evm()` (`types::ChainId::evm_chain_id`, see its doc comment:
/// "`None` for any other namespace ... whose reference is also numeric but is
/// emphatically not an EIP-155 id"), so the collision can't leak `tron:...`
/// through as if it were the eip155 chain of the same number - confirmed
/// directly here rather than only inferred from that doc comment.
#[test]
fn evm_chain_id_does_not_leak_across_the_tron_eip155_number_collision() {
    let tron = ChainId::parse("tron:728126428").unwrap();
    assert_eq!(tron.evm_chain_id(), None);
}

// =========================================================================
// unsupported_chain_error
// =========================================================================

/// The wiring the predicate alone can't prove: the 400 the handlers actually
/// send names the chain and says `unsupported_chain`, not just "some 400".
/// A refusal that 400s for the wrong reason (or a wrong field name) would
/// pass every `chain_has_no_adapter` test above and still fail a merchant
/// trying to read the error.
#[tokio::test]
async fn unsupported_chain_error_names_the_chain() {
    let tron = ChainId::parse("tron:728126428").unwrap();
    let (status, body) = body_of(unsupported_chain_error(&tron)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body,
        "unsupported_chain: no adapter is registered for tron:728126428"
    );
}

// =========================================================================
// update_should_check_chain
// =========================================================================

/// Disabling a legacy bad row must always be reachable through the API -
/// otherwise the only remediation left is direct database surgery.
#[test]
fn disabling_skips_the_chain_check() {
    assert!(!update_should_check_chain(Some(false)));
}

/// Re-enabling a legacy bad row is still refused - only turning one off is
/// safe.
#[test]
fn enabling_still_checks_the_chain() {
    assert!(update_should_check_chain(Some(true)));
}

/// An update that doesn't touch `enabled` at all (e.g. rotating the xpub)
/// must still be checked - omitting the field is not the same as disabling.
#[test]
fn an_unspecified_enabled_still_checks_the_chain() {
    assert!(update_should_check_chain(None));
}

// =========================================================================
// create_payment_method - through the handler, against a real database
//
// Every test above calls `chain_has_no_adapter`/`update_should_check_chain`
// directly, so none would notice a broken call site in the real handler -
// the "guard written but never exercised" gap. These go through the handler
// itself against the real `rcs-test-postgres` fixture instead, `#[ignore]`d
// and skipped with no `DATABASE_URL` like every other DB-backed test here.
//
// `AuthenticatedUser`/`StoreScopedUser` are built by hand rather than via
// `FromRequestParts`: `require_store_settings_permission` returns `Ok(())`
// for `Role::ServerAdmin` before touching the database, so no session or
// membership row is needed. `NoAuthSessionService` only exists to give
// `PgAppState<A>` a concrete `A`; `create_payment_method` never calls it.
//
// Ablated locally: deleting the `if chain_has_no_adapter(...)` block in
// `create_payment_method` turned the tron test's `expect_err` into an
// `Ok(StatusCode::CREATED, ...)` - proving it's sensitive to the gate, not
// some unrelated 400. Restored before committing.
// =========================================================================

use crate::api::extractors::{AuthenticatedUser, StoreScopedUser};
use crate::state::PgAppState;
use auth::{Role, SessionId, SessionService, UserInfo};
use axum::Json;
use axum::extract::{Path, State};

/// Exists only to give `PgAppState<A>` a concrete auth-service type; never
/// called because `AuthenticatedUser` below is constructed directly.
struct NoAuthSessionService;

#[async_trait::async_trait]
impl SessionService for NoAuthSessionService {
    async fn validate_session(
        &self,
        _session_id: SessionId,
    ) -> auth::Result<(UserInfo, auth::Session)> {
        Err(auth::AuthError::InvalidCredentials)
    }

    async fn logout(&self, _session_id: SessionId) -> auth::Result<()> {
        Err(auth::AuthError::InvalidCredentials)
    }

    async fn logout_all(&self, _session_id: SessionId) -> auth::Result<()> {
        Err(auth::AuthError::InvalidCredentials)
    }

    async fn cleanup_stale_sessions(&self) -> auth::Result<u64> {
        Err(auth::AuthError::InvalidCredentials)
    }
}

fn admin_user(user_id: Uuid) -> AuthenticatedUser {
    AuthenticatedUser(UserInfo {
        id: UserId(user_id),
        email: None,
        primary_wallet_address: None,
        created_at: Utc::now(),
        last_login_at: None,
        role: Role::ServerAdmin,
    })
}

/// `admin_user`, unscoped - these tests hit the ServerAdmin bypass in
/// `require_store_settings_permission`, not the key-intersection path.
fn admin_scoped(user_id: Uuid) -> StoreScopedUser {
    StoreScopedUser(admin_user(user_id).0, None)
}

async fn seed_handler_test_user(pool: &sqlx::PgPool) -> Uuid {
    let user_id = Uuid::new_v4();
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

async fn seed_handler_test_store(pool: &sqlx::PgPool, owner: Uuid) -> Uuid {
    let store_id = Uuid::new_v4();
    sqlx::query("INSERT INTO stores (id, name, owner_id) VALUES ($1, $2, $3)")
        .bind(store_id)
        .bind(format!("store-{store_id}"))
        .bind(owner)
        .execute(pool)
        .await
        .expect("seed store");
    store_id
}

/// So the create call's xpub-less branch resolves to something, without a
/// real BIP-32 key: the `wallets.xpub` column is text the repository never
/// parses (see `data-service/src/postgres/integration_tests/wallet.rs`'s
/// `unique_xpub`) - only `create_payment_method`'s own `validate_xpub` check
/// parses the request body's xpub, and this bypasses that by seeding the
/// store's resolution directly. Unique per call so repeated runs against the
/// shared fixture don't collide on the account-uniqueness constraint that
/// `wallet_for_store_xpub` enforces on this same table.
async fn seed_handler_test_primary_wallet(pool: &sqlx::PgPool, owner: Uuid) {
    sqlx::query("INSERT INTO wallets (id, user_id, xpub, is_primary) VALUES ($1, $2, $3, true)")
        .bind(Uuid::new_v4())
        .bind(owner)
        .bind(format!("xpub-test-{}", Uuid::new_v4()))
        .execute(pool)
        .await
        .expect("seed wallet");
}

fn handler_test_state(service: data_service::PgDataService) -> PgAppState<NoAuthSessionService> {
    PgAppState::new(
        std::sync::Arc::new(service),
        std::sync::Arc::new(NoAuthSessionService),
        None,
        std::sync::Arc::new(rates::NoOpRateProvider),
        std::sync::Arc::new(crate::services::email::NoopEmailSender),
    )
}

/// An account-level (depth 3) EVM xpub, the one `evm::wallet`'s tests pin to
/// the published first address. Depth matters: the handlers refuse any other.
/// Passed explicitly so the request takes the "pin to this key" branch of `create_payment_method` rather than
/// the "resolve the store's existing wallet" branch - the chain gate must
/// refuse tron before either branch runs.
const HANDLER_TEST_XPUB: &str = "xpub6DCoCpSuQZB2jawqnGMEPS63ePKWkwWPH4TU45Q7LPXWuNd8TMtVxRrgjtEshuqpK3mdhaWHPFsBngh5GFZaM6si3yZdUsT8ddYM3PwnATt";

/// `tron:728126428` must 400, naming the chain: unlike Sepolia below, it
/// would otherwise be created pinned to `HANDLER_TEST_XPUB`, and an invoice
/// against it would derive a `0x...` address from that xpub regardless of
/// namespace (`invoices/payment_options.rs`'s `derive_payment_address`
/// builds an `evm::XpubDeriver` unconditionally) with nothing to watch it.
#[tokio::test]
#[ignore]
async fn a_tron_payment_method_is_refused_by_the_handler() {
    let service = pg_service().await;
    let pool = service.pool().clone();
    let user_id = seed_handler_test_user(&pool).await;
    let store_id = seed_handler_test_store(&pool, user_id).await;
    let state = handler_test_state(service);

    let req = CreatePaymentMethodRequest {
        chain_id: ChainId::parse("tron:728126428").unwrap(),
        token_address: None,
        asset_symbol: "USDT".to_string(),
        decimals: 6,
        xpub: Some(HANDLER_TEST_XPUB.to_string()),
    };

    let err = create_payment_method(
        admin_scoped(user_id),
        State(state),
        Path(store_id),
        Json(req),
    )
    .await
    .expect_err("tron has no adapter and must be refused, not created");

    let (status, body) = body_of(err).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("unsupported_chain"), "wrong reason: {body}");
    assert!(
        body.contains("tron:728126428"),
        "the error must name the refused chain: {body}"
    );
}

/// Test 2 of the ticket: `eip155:11155111` (Sepolia) is unchanged - still 201.
#[tokio::test]
#[ignore]
async fn a_sepolia_payment_method_is_still_created_by_the_handler() {
    let service = pg_service().await;
    let pool = service.pool().clone();
    let user_id = seed_handler_test_user(&pool).await;
    let store_id = seed_handler_test_store(&pool, user_id).await;
    seed_handler_test_primary_wallet(&pool, user_id).await;
    let state = handler_test_state(service);

    let req = CreatePaymentMethodRequest {
        chain_id: ChainId::parse("eip155:11155111").unwrap(),
        token_address: None,
        asset_symbol: "ETH".to_string(),
        decimals: 18,
        xpub: None,
    };

    let (status, response) = create_payment_method(
        admin_scoped(user_id),
        State(state),
        Path(store_id),
        Json(req),
    )
    .await
    .expect("sepolia has a compiled-in adapter and must still be accepted");

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(
        response.0.chain_id,
        ChainId::parse("eip155:11155111").unwrap()
    );
}

/// The receive branch (`m/44'/60'/0'/0`) of the same mnemonic as
/// `HANDLER_TEST_XPUB`, depth 4: it parses, and the deriver would append
/// `0/i` to it and produce addresses the merchant's wallet never shows.
const HANDLER_TEST_CHILD_XPUB: &str = "xpub6EF8jXqFeFEW5bwMU7RpQtHkzE4KJxcqJtvkCjJumzW8CPpacXkb92ek4WzLQXjL93HycJwTPUAcuNxCqFPKKU5m5Z2Vq4nCyh5CyPeBFFr";

/// Create must refuse the child-depth key with the depth named, before
/// anything is stored - and the account key on the same chain still passes.
#[tokio::test]
#[ignore]
async fn creating_a_payment_method_with_a_child_depth_xpub_is_refused() {
    let service = pg_service().await;
    let pool = service.pool().clone();
    let user_id = seed_handler_test_user(&pool).await;
    let store_id = seed_handler_test_store(&pool, user_id).await;
    let state = handler_test_state(service);
    // Unique per run: an xpub may belong to only one account and this test
    // shares a database with every other.
    let fresh_account_xpub = evm::HdWallet::from_mnemonic(&evm::generate_mnemonic(12).unwrap(), "")
        .unwrap()
        .account_xpub_string_for(evm::ChainFamily::Evm)
        .unwrap();

    let req = |xpub: &str| CreatePaymentMethodRequest {
        chain_id: ChainId::parse("eip155:11155111").unwrap(),
        token_address: None,
        asset_symbol: "ETH".to_string(),
        decimals: 18,
        xpub: Some(xpub.to_string()),
    };

    let err = create_payment_method(
        admin_scoped(user_id),
        State(state.clone()),
        Path(store_id),
        Json(req(HANDLER_TEST_CHILD_XPUB)),
    )
    .await
    .expect_err("a depth-4 key must be refused");
    let (status, body) = body_of(err).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("depth 4"), "must name the depth: {body}");

    let (status, _) = create_payment_method(
        admin_scoped(user_id),
        State(state),
        Path(store_id),
        Json(req(&fresh_account_xpub)),
    )
    .await
    .expect("the account-level key must still be accepted");
    assert_eq!(status, StatusCode::CREATED);
}

/// Update must refuse it too: the method need not exist, because the key is
/// judged before the method is looked up.
#[tokio::test]
#[ignore]
async fn updating_a_payment_method_to_a_child_depth_xpub_is_refused() {
    let service = pg_service().await;
    let pool = service.pool().clone();
    let user_id = seed_handler_test_user(&pool).await;
    let store_id = seed_handler_test_store(&pool, user_id).await;
    let state = handler_test_state(service);

    let err = update_payment_method(
        admin_scoped(user_id),
        State(state),
        Path((store_id, Uuid::new_v4())),
        Json(UpdatePaymentMethodRequest {
            enabled: None,
            xpub: Some(HANDLER_TEST_CHILD_XPUB.to_string()),
        }),
    )
    .await
    .expect_err("a depth-4 key must be refused");
    let (status, body) = body_of(err).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("depth 4"), "must name the depth: {body}");
}

/// Rotation must refuse it as well, with nothing moved.
#[tokio::test]
#[ignore]
async fn rotating_a_store_wallet_to_a_child_depth_xpub_is_refused() {
    let service = pg_service().await;
    let pool = service.pool().clone();
    let user_id = seed_handler_test_user(&pool).await;
    let store_id = seed_handler_test_store(&pool, user_id).await;
    let state = handler_test_state(service);

    let err = rotate_store_wallet(
        admin_scoped(user_id),
        State(state),
        Path(store_id),
        Json(RotateWalletRequest {
            xpub: HANDLER_TEST_CHILD_XPUB.to_string(),
            reason: None,
            namespace: "eip155".to_string(),
        }),
    )
    .await
    .expect_err("a depth-4 key must be refused");
    let (status, body) = body_of(err).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("depth 4"), "must name the depth: {body}");
}

// =========================================================================
// Request deserialization
// =========================================================================

#[test]
fn test_create_store_request() {
    let json = r#"{"name": "My Store", "website": "https://mystore.com"}"#;
    let req: CreateStoreRequest = serde_json::from_str(json).unwrap();
    assert_eq!(req.name, "My Store");
    assert_eq!(req.website, Some("https://mystore.com".to_string()));
}

#[test]
fn test_create_store_request_without_website() {
    let json = r#"{"name": "My Store"}"#;
    let req: CreateStoreRequest = serde_json::from_str(json).unwrap();
    assert_eq!(req.name, "My Store");
    assert!(req.website.is_none());
}

#[test]
fn test_create_store_request_missing_name() {
    let json = r#"{"website": "https://mystore.com"}"#;
    let result = serde_json::from_str::<CreateStoreRequest>(json);
    assert!(result.is_err());
}

#[test]
fn test_update_store_request_partial() {
    let json = r#"{"name": "New Name"}"#;
    let req: UpdateStoreRequest = serde_json::from_str(json).unwrap();
    assert_eq!(req.name, Some("New Name".to_string()));
    assert!(req.website.is_none());
}

#[test]
fn test_update_store_request_empty() {
    let json = r#"{}"#;
    let req: UpdateStoreRequest = serde_json::from_str(json).unwrap();
    assert!(req.name.is_none());
    assert!(req.website.is_none());
}

#[test]
fn test_create_payment_method_request() {
    let json = r#"{
        "chain_id": "eip155:1",
        "token_address": null,
        "asset_symbol": "ETH",
        "decimals": 18,
        "xpub": "xpub123..."
    }"#;
    let req: CreatePaymentMethodRequest = serde_json::from_str(json).unwrap();
    assert_eq!(req.chain_id, ChainId::parse("eip155:1").unwrap());
    assert!(req.token_address.is_none());
    assert_eq!(req.asset_symbol, "ETH");
    assert_eq!(req.decimals, 18);
}

#[test]
fn test_update_payment_method_request_partial() {
    let json = r#"{"enabled": false}"#;
    let req: UpdatePaymentMethodRequest = serde_json::from_str(json).unwrap();
    assert_eq!(req.enabled, Some(false));
    assert!(req.xpub.is_none());
}

#[test]
fn test_configure_webhook_request_defaults_enabled() {
    let json = r#"{"webhook_url": "https://example.com/hook"}"#;
    let req: ConfigureWebhookRequest = serde_json::from_str(json).unwrap();
    assert_eq!(req.webhook_url, "https://example.com/hook");
    assert!(req.enabled);
}

#[test]
fn test_configure_webhook_request_disabled() {
    let json = r#"{"webhook_url": "https://example.com/hook", "enabled": false}"#;
    let req: ConfigureWebhookRequest = serde_json::from_str(json).unwrap();
    assert!(!req.enabled);
}

// =========================================================================
// Response serialization
// =========================================================================

#[test]
fn test_store_response_json() {
    let response = StoreResponse {
        id: Uuid::nil(),
        name: "Test".to_string(),
        website: None,
        owner_id: Uuid::nil(),
        archived: false,
        created_at: Utc::now(),
    };

    let json = serde_json::to_value(&response).unwrap();
    assert_eq!(json["name"], "Test");
    assert_eq!(json["archived"], false);
    assert!(json["website"].is_null());
    assert!(json["id"].is_string());
    assert!(json["created_at"].is_string());
}

#[test]
fn test_member_response_json() {
    let response = MemberResponse {
        user_id: Uuid::nil(),
        store_id: Uuid::nil(),
        role_id: Uuid::nil(),
        role_name: "Owner".to_string(),
        permissions: vec![
            "ethpay.store.canmodifystoresettings".to_string(),
            "ethpay.store.canviewinvoices".to_string(),
        ],
    };

    let json = serde_json::to_value(&response).unwrap();
    assert_eq!(json["role_name"], "Owner");
    assert_eq!(json["permissions"].as_array().unwrap().len(), 2);
}

#[test]
fn test_wallet_response_json() {
    let response = WalletResponse {
        id: Uuid::nil(),
        user_id: Uuid::nil(),
        namespace: "eip155".to_string(),
        xpub_masked: "xpub6CUG...3fDVmz".to_string(),
        derivation_index: 42,
        is_primary: false,
        name: Some("Main Wallet".to_string()),
        created_at: Utc::now(),
    };

    let json = serde_json::to_value(&response).unwrap();
    assert_eq!(json["derivation_index"], 42);
    assert_eq!(json["name"], "Main Wallet");
    assert_eq!(json["xpub_masked"], "xpub6CUG...3fDVmz");
}

#[test]
fn test_webhook_response_with_secret() {
    let response = WebhookResponse {
        id: Uuid::nil(),
        store_id: Uuid::nil(),
        webhook_url: "https://example.com/webhook".to_string(),
        webhook_secret: Some("secret123".to_string()),
        enabled: true,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };

    let json = serde_json::to_value(&response).unwrap();
    assert_eq!(json["webhook_url"], "https://example.com/webhook");
    assert_eq!(json["webhook_secret"], "secret123");
    assert!(json["enabled"].as_bool().unwrap());
}

#[test]
fn test_webhook_response_without_secret() {
    let response = WebhookResponse {
        id: Uuid::nil(),
        store_id: Uuid::nil(),
        webhook_url: "https://example.com/webhook".to_string(),
        webhook_secret: None,
        enabled: true,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };

    let json = serde_json::to_value(&response).unwrap();
    assert!(json["webhook_secret"].is_null());
}

// =========================================================================
// list_wallets response shape
// =========================================================================

#[test]
fn test_list_wallets_response_serialization() {
    let wallets = vec![
        WalletResponse {
            id: Uuid::nil(),
            user_id: Uuid::nil(),
            namespace: "eip155".to_string(),
            xpub_masked: mask_xpub(
                "xpub6DCoCpSuQZB2jawqnGMEPS63ePKWkwWPH4TU45Q7LPXWuNd8TMtVxRrgjtEshuqpK3mdhaWHPFsBngh5GFZaM6si3yZdUsT8ddYM3PwnATt",
            ),
            derivation_index: 0,
            is_primary: false,
            name: Some("ETH Wallet".to_string()),
            created_at: Utc::now(),
        },
        WalletResponse {
            id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            namespace: "eip155".to_string(),
            xpub_masked: mask_xpub(
                "xpub6D4BDPcP2GT577Vvch3R8wDkScZWzQzMMUm3PWbmWvVJrZwQY4VUNgqFJPMM3No2dFDFGTsxxpG5uJh7n7epu4trkrX7x7DogT5Uv6fcLW5",
            ),
            derivation_index: 5,
            is_primary: false,
            name: None,
            created_at: Utc::now(),
        },
    ];

    let json = serde_json::to_value(&wallets).unwrap();
    let arr = json.as_array().unwrap();
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["name"], "ETH Wallet");
    assert_eq!(arr[0]["derivation_index"], 0);
    assert!(arr[0]["xpub_masked"].as_str().unwrap().contains("..."));
    assert!(arr[1]["name"].is_null());
    assert_eq!(arr[1]["derivation_index"], 5);
}

#[test]
fn test_list_wallets_empty_response() {
    let wallets: Vec<WalletResponse> = vec![];
    let json = serde_json::to_value(&wallets).unwrap();
    assert!(json.as_array().unwrap().is_empty());
}

// =========================================================================
// get_wallet_by_id response
// =========================================================================

#[test]
fn test_wallet_by_id_response_masks_xpub() {
    let xpub = "xpub6DCoCpSuQZB2jawqnGMEPS63ePKWkwWPH4TU45Q7LPXWuNd8TMtVxRrgjtEshuqpK3mdhaWHPFsBngh5GFZaM6si3yZdUsT8ddYM3PwnATt";
    let response = WalletResponse {
        id: Uuid::new_v4(),
        user_id: Uuid::new_v4(),
        namespace: "eip155".to_string(),
        xpub_masked: mask_xpub(xpub),
        derivation_index: 7,
        is_primary: false,
        name: Some("Hot Wallet".to_string()),
        created_at: Utc::now(),
    };

    let json = serde_json::to_value(&response).unwrap();
    let masked = json["xpub_masked"].as_str().unwrap();
    assert!(masked.contains("..."));
    assert!(!masked.contains(xpub));
    assert_eq!(json["derivation_index"], 7);
    assert_eq!(json["name"], "Hot Wallet");
}

#[test]
fn test_wallet_by_id_response_without_name() {
    let response = WalletResponse {
        id: Uuid::nil(),
        user_id: Uuid::nil(),
        namespace: "eip155".to_string(),
        xpub_masked: "xpub6CUG...3fDVmz".to_string(),
        derivation_index: 0,
        is_primary: false,
        name: None,
        created_at: Utc::now(),
    };

    let json = serde_json::to_value(&response).unwrap();
    assert!(json["name"].is_null());
    assert_eq!(json["derivation_index"], 0);
}

#[test]
fn test_wallet_by_id_response_contains_user_id() {
    let user_id = Uuid::new_v4();
    let response = WalletResponse {
        id: Uuid::new_v4(),
        user_id,
        namespace: "eip155".to_string(),
        xpub_masked: "xpub6D4B...cLW5".to_string(),
        derivation_index: 3,
        is_primary: false,
        name: Some("Cold Storage".to_string()),
        created_at: Utc::now(),
    };

    let json = serde_json::to_value(&response).unwrap();
    assert_eq!(json["user_id"].as_str().unwrap(), user_id.to_string());
}

// =========================================================================
// export_wallet_xpub response
// =========================================================================

#[test]
fn test_xpub_export_response_contains_full_xpub() {
    let xpub = "xpub6DCoCpSuQZB2jawqnGMEPS63ePKWkwWPH4TU45Q7LPXWuNd8TMtVxRrgjtEshuqpK3mdhaWHPFsBngh5GFZaM6si3yZdUsT8ddYM3PwnATt";
    let response = WalletXpubResponse {
        id: Uuid::new_v4(),
        user_id: Uuid::new_v4(),
        namespace: "eip155".to_string(),
        xpub: xpub.to_string(),
        derivation_index: 5,
        name: Some("Main Wallet".to_string()),
        created_at: Utc::now(),
    };

    let json = serde_json::to_value(&response).unwrap();
    assert_eq!(json["xpub"], xpub);
    assert!(!json["xpub"].as_str().unwrap().contains("..."));
    assert_eq!(json["derivation_index"], 5);
    assert_eq!(json["name"], "Main Wallet");
}

#[test]
fn test_xpub_export_response_without_name() {
    let response = WalletXpubResponse {
        id: Uuid::nil(),
        user_id: Uuid::nil(),
        namespace: "eip155".to_string(),
        xpub: "xpub6D4BDPcP2GT577Vvch3R8wDkScZWzQzMMUm3PWbmWvVJrZwQY4VUNgqFJPMM3No2dFDFGTsxxpG5uJh7n7epu4trkrX7x7DogT5Uv6fcLW5".to_string(),
        derivation_index: 0,
        name: None,
        created_at: Utc::now(),
    };

    let json = serde_json::to_value(&response).unwrap();
    assert!(json["name"].is_null());
    assert!(json["xpub"].as_str().unwrap().starts_with("xpub"));
}

// =========================================================================
// list_wallet_addresses response
// =========================================================================

#[test]
fn test_derived_address_entry_serialization() {
    let entry = DerivedAddressEntry {
        address: "0x1234567890abcdef1234567890abcdef12345678".to_string(),
        index: 0,
        derivation_path: "m/44'/60'/0'/0/0".to_string(),
        used: true,
    };

    let json = serde_json::to_value(&entry).unwrap();
    assert_eq!(json["index"], 0);
    assert_eq!(json["derivation_path"], "m/44'/60'/0'/0/0");
    assert_eq!(json["used"], true);
    assert!(json["address"].as_str().unwrap().starts_with("0x"));
}

#[test]
fn test_wallet_addresses_response_marks_used_correctly() {
    let response = WalletAddressesResponse {
        wallet_id: Uuid::new_v4(),
        derivation_index: 3,
        addresses: vec![
            DerivedAddressEntry {
                address: "0xaaa0".to_string(),
                index: 0,
                derivation_path: "m/44'/60'/0'/0/0".to_string(),
                used: true,
            },
            DerivedAddressEntry {
                address: "0xaaa1".to_string(),
                index: 1,
                derivation_path: "m/44'/60'/0'/0/1".to_string(),
                used: true,
            },
            DerivedAddressEntry {
                address: "0xaaa2".to_string(),
                index: 2,
                derivation_path: "m/44'/60'/0'/0/2".to_string(),
                used: true,
            },
            DerivedAddressEntry {
                address: "0xaaa3".to_string(),
                index: 3,
                derivation_path: "m/44'/60'/0'/0/3".to_string(),
                used: false,
            },
            DerivedAddressEntry {
                address: "0xaaa4".to_string(),
                index: 4,
                derivation_path: "m/44'/60'/0'/0/4".to_string(),
                used: false,
            },
        ],
    };

    let json = serde_json::to_value(&response).unwrap();
    assert_eq!(json["derivation_index"], 3);
    let addrs = json["addresses"].as_array().unwrap();
    assert_eq!(addrs.len(), 5);
    // First 3 (index < derivation_index=3) should be used
    assert_eq!(addrs[0]["used"], true);
    assert_eq!(addrs[1]["used"], true);
    assert_eq!(addrs[2]["used"], true);
    // Index 3 and 4 should be unused
    assert_eq!(addrs[3]["used"], false);
    assert_eq!(addrs[4]["used"], false);
}

#[test]
fn test_wallet_addresses_response_empty() {
    let response = WalletAddressesResponse {
        wallet_id: Uuid::new_v4(),
        derivation_index: 0,
        addresses: vec![],
    };

    let json = serde_json::to_value(&response).unwrap();
    assert!(json["addresses"].as_array().unwrap().is_empty());
    assert_eq!(json["derivation_index"], 0);
}

// =========================================================================
// Store Settings validation tests
// =========================================================================

#[test]
fn test_store_settings_response_serialization() {
    let response = StoreSettingsResponse {
        store_id: Uuid::nil(),
        default_chain_id: Some(ChainId::evm(137)),
        default_display_currency: Some("USD".to_string()),
        logo_url: Some("https://example.com/logo.png".to_string()),
        accent_color: Some("#FF5500".to_string()),
        notification_prefs: serde_json::json!({"payment_confirmed": {"webhook": true}}),
        updated_at: "2026-04-20T00:00:00Z".to_string(),
    };
    let json = serde_json::to_value(&response).unwrap();
    assert_eq!(json["default_chain_id"], "eip155:137");
    assert_eq!(json["default_display_currency"], "USD");
    assert_eq!(json["logo_url"], "https://example.com/logo.png");
    assert_eq!(json["accent_color"], "#FF5500");
}

#[test]
fn test_store_settings_response_defaults() {
    let response = StoreSettingsResponse {
        store_id: Uuid::nil(),
        default_chain_id: None,
        default_display_currency: None,
        logo_url: None,
        accent_color: None,
        notification_prefs: serde_json::json!({}),
        updated_at: "2026-04-20T00:00:00Z".to_string(),
    };
    let json = serde_json::to_value(&response).unwrap();
    assert!(json["default_chain_id"].is_null());
    assert!(json["logo_url"].is_null());
}

// =========================================================================
// notification_prefs: validation and merge
//
// The bug these pin: `customer_receipts_enabled` shares the notification_prefs
// blob with the five event keys, but validation knew only about the events, so
// any payload containing it was a 400. A client could therefore only save
// notification preferences by dropping the key - and the update replaced the
// blob wholesale, so dropping it removed it. `receipts_disabled_for_store`
// reads absent as ENABLED, so a merchant who had switched customer emails off
// started sending them again by saving an unrelated preference.
// =========================================================================

#[test]
fn switch_key_is_accepted() {
    // The exact payload that used to 400.
    assert!(
        validate_notification_prefs(&serde_json::json!({"customer_receipts_enabled": false}))
            .is_ok()
    );
    assert!(
        validate_notification_prefs(&serde_json::json!({"customer_receipts_enabled": true}))
            .is_ok()
    );
}

#[test]
fn event_and_switch_can_be_sent_together() {
    assert!(
        validate_notification_prefs(&serde_json::json!({
            "payment_confirmed": {"webhook": true},
            "customer_receipts_enabled": false
        }))
        .is_ok()
    );
}

#[test]
fn a_switch_sent_as_a_string_is_refused() {
    // Would store cleanly and then read as ENABLED: `receipts_disabled_for_store`
    // matches Bool(false) exactly, so "false" is not off.
    assert!(
        validate_notification_prefs(&serde_json::json!({"customer_receipts_enabled": "false"}))
            .is_err()
    );
    assert!(
        validate_notification_prefs(&serde_json::json!({"customer_receipts_enabled": 0})).is_err()
    );
}

#[test]
fn an_event_sent_as_a_bool_is_refused() {
    // Events carry a channel map; a bare bool would silently disable nothing.
    assert!(validate_notification_prefs(&serde_json::json!({"payment_confirmed": true})).is_err());
}

#[test]
fn unknown_keys_are_still_refused() {
    assert!(validate_notification_prefs(&serde_json::json!({"not_a_real_key": true})).is_err());
}

#[test]
fn a_non_object_blob_is_refused() {
    assert!(validate_notification_prefs(&serde_json::json!("nope")).is_err());
    assert!(validate_notification_prefs(&serde_json::json!([])).is_err());
}

#[test]
fn saving_an_unrelated_preference_leaves_receipts_off() {
    // THE regression. Receipts explicitly off; the merchant saves a webhook
    // preference, which is a payload that does not mention receipts at all.
    let stored = serde_json::json!({
        "customer_receipts_enabled": false,
        "payment_detected": {"webhook": true}
    });
    let patch = serde_json::json!({"payment_confirmed": {"webhook": true}});

    let merged = merge_notification_prefs(&stored, &patch);

    assert_eq!(
        merged["customer_receipts_enabled"],
        serde_json::Value::Bool(false),
        "saving an unrelated preference must not resume emailing the merchant's customers"
    );
    assert_eq!(
        merged["payment_detected"],
        serde_json::json!({"webhook": true})
    );
    assert_eq!(
        merged["payment_confirmed"],
        serde_json::json!({"webhook": true})
    );
}

#[test]
fn a_patch_overrides_the_stored_value() {
    let stored = serde_json::json!({"customer_receipts_enabled": false});
    let merged = merge_notification_prefs(
        &stored,
        &serde_json::json!({"customer_receipts_enabled": true}),
    );
    assert_eq!(
        merged["customer_receipts_enabled"],
        serde_json::Value::Bool(true)
    );
}

#[test]
fn an_explicit_null_removes_a_key() {
    // Merging means an omitted key is kept, so there has to be a way to unset.
    let stored =
        serde_json::json!({"customer_receipts_enabled": false, "late_paid": {"webhook": true}});
    let merged = merge_notification_prefs(&stored, &serde_json::json!({"late_paid": null}));
    assert!(merged.get("late_paid").is_none());
    assert_eq!(
        merged["customer_receipts_enabled"],
        serde_json::Value::Bool(false)
    );
}

#[test]
fn an_empty_patch_changes_nothing() {
    let stored =
        serde_json::json!({"customer_receipts_enabled": false, "late_paid": {"webhook": true}});
    let merged = merge_notification_prefs(&stored, &serde_json::json!({}));
    assert_eq!(merged, stored);
}

#[test]
fn merging_onto_an_empty_blob_keeps_the_patch() {
    let merged = merge_notification_prefs(
        &serde_json::json!({}),
        &serde_json::json!({"customer_receipts_enabled": false}),
    );
    assert_eq!(
        merged["customer_receipts_enabled"],
        serde_json::Value::Bool(false)
    );
}

#[test]
fn test_valid_notification_switches_list() {
    assert_eq!(VALID_NOTIFICATION_SWITCHES.len(), 1);
    assert!(VALID_NOTIFICATION_SWITCHES.contains(&"customer_receipts_enabled"));
}

#[test]
fn test_valid_notification_events_list() {
    assert_eq!(VALID_NOTIFICATION_EVENTS.len(), 6);
    assert!(VALID_NOTIFICATION_EVENTS.contains(&"payment_detected"));
    assert!(VALID_NOTIFICATION_EVENTS.contains(&"payment_confirmed"));
    assert!(VALID_NOTIFICATION_EVENTS.contains(&"payment_reorged"));
    assert!(VALID_NOTIFICATION_EVENTS.contains(&"invoice_expired"));
    assert!(VALID_NOTIFICATION_EVENTS.contains(&"invoice_cancelled"));
    assert!(VALID_NOTIFICATION_EVENTS.contains(&"late_paid"));
}

/// The preference list and the event vocabulary are one thing. An event absent
/// from the list cannot be switched off; a name in the list that no event uses
/// is a switch wired to nothing. Both have happened.
#[test]
fn test_every_webhook_event_is_configurable() {
    use crate::services::webhook::WebhookEventType;

    let events = [
        WebhookEventType::PaymentDetected,
        WebhookEventType::PaymentConfirmed,
        WebhookEventType::PaymentReorged,
        WebhookEventType::InvoiceExpired,
        WebhookEventType::InvoiceCancelled,
        WebhookEventType::LatePaid,
    ];

    for event in events {
        assert!(
            VALID_NOTIFICATION_EVENTS.contains(&event.as_str()),
            "{event} cannot be switched off in notification_prefs"
        );
    }

    // The array above is exhaustive over the enum: if a variant is added
    // without being listed here, this fails and the match below stops
    // compiling.
    assert_eq!(VALID_NOTIFICATION_EVENTS.len(), events.len());
    for event in events {
        match event {
            WebhookEventType::PaymentDetected
            | WebhookEventType::PaymentConfirmed
            | WebhookEventType::PaymentReorged
            | WebhookEventType::InvoiceExpired
            | WebhookEventType::InvoiceCancelled
            | WebhookEventType::LatePaid => {}
        }
    }
}

#[test]
fn test_update_settings_request_deserialization() {
    let json = serde_json::json!({
        "default_chain_id": "eip155:1",
        "default_display_currency": "EUR",
        "logo_url": "https://example.com/logo.png",
        "accent_color": "#00FF00",
        "notification_prefs": {"payment_detected": {"webhook": false}}
    });
    let req: UpdateStoreSettingsRequest = serde_json::from_value(json).unwrap();
    assert_eq!(req.default_chain_id, Some(ChainId::evm(1)));
    assert_eq!(req.default_display_currency.as_deref(), Some("EUR"));
    assert_eq!(
        req.logo_url.as_deref(),
        Some("https://example.com/logo.png")
    );
    assert_eq!(req.accent_color.as_deref(), Some("#00FF00"));
}

#[test]
fn test_update_settings_request_partial() {
    let json = serde_json::json!({"accent_color": "#AABBCC"});
    let req: UpdateStoreSettingsRequest = serde_json::from_value(json).unwrap();
    assert!(req.default_chain_id.is_none());
    assert!(req.default_display_currency.is_none());
    assert!(req.logo_url.is_none());
    assert_eq!(req.accent_color.as_deref(), Some("#AABBCC"));
    assert!(req.notification_prefs.is_none());
}

// =========================================================================
// Wallet rotation
// =========================================================================

#[test]
fn test_rotate_wallet_request_deserialization() {
    let json = r#"{"xpub": "xpub6DCoCpSuQZB2jawqnGMEPS63ePKWkwWPH4TU45Q7LPXWuNd8TMtVxRrgjtEshuqpK3mdhaWHPFsBngh5GFZaM6si3yZdUsT8ddYM3PwnATt", "reason": "key compromise"}"#;
    let req: RotateWalletRequest = serde_json::from_str(json).unwrap();
    assert!(req.xpub.starts_with("xpub"));
    assert_eq!(req.reason, Some("key compromise".to_string()));
}

#[test]
fn test_rotate_wallet_request_without_reason() {
    let json = r#"{"xpub": "xpub6DCoCpSuQZB2jawqnGMEPS63ePKWkwWPH4TU45Q7LPXWuNd8TMtVxRrgjtEshuqpK3mdhaWHPFsBngh5GFZaM6si3yZdUsT8ddYM3PwnATt"}"#;
    let req: RotateWalletRequest = serde_json::from_str(json).unwrap();
    assert!(req.xpub.starts_with("xpub"));
    assert!(req.reason.is_none());
}

#[test]
fn test_rotate_wallet_request_missing_xpub() {
    let json = r#"{"reason": "test"}"#;
    let result = serde_json::from_str::<RotateWalletRequest>(json);
    assert!(result.is_err());
}

#[test]
fn test_rotate_wallet_response_serialization() {
    let response = RotateWalletResponse {
        store_id: Uuid::nil(),
        new_xpub_masked: "xpub6CUG...3fDVmz".to_string(),
        methods_rotated: 2,
        rotations: vec![
            RotationEntry {
                id: Uuid::new_v4(),
                payment_method_id: Uuid::new_v4(),
                chain_id: Some(ChainId::evm(1)),
                asset_symbol: Some("ETH".to_string()),
                previous_xpub_masked: "xpub6D4B...cLW5".to_string(),
                previous_derivation_index: 5,
                rotated_at: Utc::now(),
            },
            RotationEntry {
                id: Uuid::new_v4(),
                payment_method_id: Uuid::new_v4(),
                chain_id: Some(ChainId::evm(137)),
                asset_symbol: Some("USDC".to_string()),
                previous_xpub_masked: "xpub6D4B...cLW5".to_string(),
                previous_derivation_index: 12,
                rotated_at: Utc::now(),
            },
        ],
    };

    let json = serde_json::to_value(&response).unwrap();
    assert_eq!(json["methods_rotated"], 2);
    assert_eq!(json["new_xpub_masked"], "xpub6CUG...3fDVmz");
    let rotations = json["rotations"].as_array().unwrap();
    assert_eq!(rotations.len(), 2);
    assert_eq!(rotations[0]["chain_id"], "eip155:1");
    assert_eq!(rotations[0]["asset_symbol"], "ETH");
    assert_eq!(rotations[0]["previous_derivation_index"], 5);
    assert_eq!(rotations[1]["chain_id"], "eip155:137");
    assert_eq!(rotations[1]["asset_symbol"], "USDC");
}

#[test]
fn test_rotate_wallet_response_empty_rotations() {
    let response = RotateWalletResponse {
        store_id: Uuid::nil(),
        new_xpub_masked: "xpub6CUG...3fDVmz".to_string(),
        methods_rotated: 0,
        rotations: vec![],
    };

    let json = serde_json::to_value(&response).unwrap();
    assert_eq!(json["methods_rotated"], 0);
    assert!(json["rotations"].as_array().unwrap().is_empty());
}

// =========================================================================
// repository_error / ApiErr
// =========================================================================

use axum::body::to_bytes;
use axum::response::IntoResponse;

async fn body_of(err: ApiErr) -> (StatusCode, String) {
    let response = err.into_response();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

/// The refusal has to say what it refused.
///
/// The client renders the body after the status (`ApiError::Http`), so a 409
/// with nothing in it reaches the merchant as the literal "HTTP error 409:" -
/// which is what the payment-method form showed, and is barely better than the
/// 500 it replaced. The merchant cannot guess "that key belongs to another
/// account" from a number.
#[tokio::test]
async fn conflict_carries_the_reason_the_caller_needs() {
    let err = repository_error(data_service::RepositoryError::Conflict(
        "this xpub is already registered to another account".into(),
    ));

    let (status, body) = body_of(err).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body, "this xpub is already registered to another account");
}

#[tokio::test]
async fn not_found_carries_its_reason_too() {
    let err = repository_error(data_service::RepositoryError::NotFound(
        "wallet not found".into(),
    ));

    let (status, body) = body_of(err).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body, "wallet not found");
}

/// Everything that is not `Conflict` or `NotFound` is answered generically.
///
/// Those two are written by this codebase for the caller to read. Any other
/// repository error may be carrying a database error, whose text names columns,
/// constraints, and - for a connection failure - the host and credentials in
/// the URL. None of that may reach a response.
#[tokio::test]
async fn other_repository_errors_say_nothing_specific() {
    let err = repository_error(data_service::RepositoryError::Database(
        "FATAL: password authentication failed for user \"ethpayserver\" at db.internal:5432"
            .into(),
    ));

    let (status, body) = body_of(err).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body, "internal error");
    assert!(
        !body.contains("db.internal"),
        "the host leaked into a response"
    );
    assert!(
        !body.contains("password"),
        "the error text leaked into a response"
    );
}

/// A bare status stays bare, and is not given a content-type for a body it
/// does not have.
///
/// Most handlers here still return `StatusCode`. Converting those through
/// `ApiErr` must leave them exactly as they were on the wire - asserting the
/// empty body alone would not catch that, since a `(status, String::new())`
/// response is empty too and differs only in its headers.
#[tokio::test]
async fn a_plain_status_stays_a_plain_status() {
    let response = ApiErr::from(StatusCode::NOT_FOUND).into_response();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(
        response.headers().get("content-type").is_none(),
        "a reasonless refusal must not claim to carry a body"
    );

    let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
    assert!(bytes.is_empty());
}

/// ...and a reason does come with one.
#[tokio::test]
async fn a_reason_is_sent_as_text() {
    let response = ApiErr::from((StatusCode::CONFLICT, "nope".to_string())).into_response();
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        content_type.starts_with("text/plain"),
        "unexpected content-type: {content_type}"
    );
}

// =========================================================================
// Chain families: what a merchant is given to check a new key with
// =========================================================================

/// The account xpub the standard BIP-39 test mnemonic exports at
/// `m/44'/195'/0'` - a Tron receiving key.
const TRON_ACCOUNT_XPUB: &str = "xpub6D1AabNHCupeiLM65ZR9UStMhJ1vCpyV4XbZdyhMZBiJXALQtmn9p42VTQckoHVn8WNqS7dqnJokZHAHcHGoaQgmv8D45oNUKx6DZMNZBCd";

/// The same seed at `m/44'/60'/0'` - an Ethereum receiving key. Note that
/// nothing in either string says which is which.
const EVM_ACCOUNT_XPUB: &str = "xpub6DCoCpSuQZB2jawqnGMEPS63ePKWkwWPH4TU45Q7LPXWuNd8TMtVxRrgjtEshuqpK3mdhaWHPFsBngh5GFZaM6si3yZdUsT8ddYM3PwnATt";

fn wallet_row(namespace: &str, xpub: &str) -> data_service::Wallet {
    data_service::Wallet {
        id: Uuid::new_v4(),
        user_id: Uuid::new_v4(),
        namespace: namespace.to_string(),
        xpub: xpub.to_string(),
        derivation_index: 0,
        name: None,
        is_primary: true,
        created_at: Utc::now(),
    }
}

/// Registering a Tron wallet hands back three `T…` addresses that match a
/// published derivation of that seed at `m/44'/195'/0'`.
///
/// This is the only defence there is. An account-level xpub cannot be asked
/// which BIP-44 coin type it was exported under, and an Ethereum one is
/// byte-indistinguishable from a Tron one, so a merchant who pastes into the
/// wrong field is told nothing - the key validates, the addresses derive, and
/// the failure surfaces when someone has already paid. Comparing the first few
/// addresses against their own wallet before any invoice quotes one is the
/// check, so these have to be right.
///
/// The values come from an independent derivation, not from this code. A
/// self-consistently wrong implementation - one that re-encodes an Ethereum
/// key in base58check and calls it Tron - produces valid `T…` addresses for
/// every index and passes any assertion phrased relative to itself.
#[test]
fn a_tron_wallet_yields_the_published_tron_addresses_to_check_it_with() {
    let wallet = wallet_row("tron", TRON_ACCOUNT_XPUB);
    let entries = derive_entries(&wallet, 0, 3).expect("derive tron addresses");

    let addresses: Vec<&str> = entries.iter().map(|e| e.address.as_str()).collect();
    assert_eq!(
        addresses,
        vec![
            "TUEZSdKsoDHQMeZwihtdoBiN46zxhGWYdH",
            "TSeJkUh4Qv67VNFwY8LaAxERygNdy6NQZK",
            "TYJPRrdB5APNeRs4R7fYZSwW3TcrTKw2gx",
        ]
    );

    // The path is shown beside each address, and the coin type in it is the
    // part the merchant's wallet has to agree with.
    let paths: Vec<&str> = entries.iter().map(|e| e.derivation_path.as_str()).collect();
    assert_eq!(
        paths,
        vec![
            "m/44'/195'/0'/0/0",
            "m/44'/195'/0'/0/1",
            "m/44'/195'/0'/0/2",
        ]
    );
    assert!(entries.iter().all(|e| !e.used));
}

/// The same seed's Ethereum key, filed as Tron, derives valid `T…` addresses
/// that are none of the above.
///
/// The failure mode, stated. It is not that the wrong key produces an error,
/// or a malformed address, or anything a merchant or this server could notice:
/// it produces three perfectly good Tron addresses at coin type 60, which
/// their wallet will never display. That is why the endpoint returns addresses
/// rather than trying to validate the key.
#[test]
fn an_ethereum_key_filed_as_tron_derives_plausible_and_wrong_addresses() {
    let misfiled = derive_entries(&wallet_row("tron", EVM_ACCOUNT_XPUB), 0, 3)
        .expect("an ethereum key filed as tron still derives");
    let correct = derive_entries(&wallet_row("tron", TRON_ACCOUNT_XPUB), 0, 3)
        .expect("derive tron addresses");

    assert!(misfiled.iter().all(|e| e.address.starts_with('T')));
    for entry in &misfiled {
        assert!(
            !correct.iter().any(|c| c.address == entry.address),
            "a coin-type-60 key produced a coin-type-195 address: {}",
            entry.address
        );
    }
}

/// An Ethereum wallet still renders hex, at coin type 60.
///
/// The path used to be hardcoded as `m/44'/60'/0'/0/{i}` regardless of the
/// key, which was a claim rather than a fact. It now comes from the wallet's
/// own family, and for `eip155` the answer has to be unchanged.
#[test]
fn an_ethereum_wallet_still_renders_hex_at_coin_type_sixty() {
    let entries = derive_entries(&wallet_row("eip155", EVM_ACCOUNT_XPUB), 0, 1)
        .expect("derive ethereum addresses");

    assert_eq!(
        entries[0].address.to_lowercase(),
        "0x9858effd232b4033e47d90003d41ec34ecaeda94"
    );
    assert_eq!(entries[0].derivation_path, "m/44'/60'/0'/0/0");
}

/// A key registered for a family this build cannot derive for is refused at
/// `POST /wallets`, before the row is written.
///
/// Storing it would leave a wallet on the merchant's account that no payment
/// method can use, listed beside working ones, with nothing to explain why.
#[tokio::test]
async fn a_namespace_this_server_cannot_derive_for_is_refused() {
    // No database is touched: the namespace check runs before any write, which
    // is the property under test.
    assert!(evm::family_for_namespace("solana").is_err());
    assert!(evm::family_for_namespace("eip155").is_ok());
    assert!(evm::family_for_namespace("tron").is_ok());
}

/// The same string means the same family to `evm` and to `types`.
///
/// `evm` spells the CAIP-2 namespaces itself rather than depending on `types`,
/// so there are two copies of `"eip155"` in the tree. This module sees both,
/// and a drift between them would resolve wallets for nothing while every test
/// in either crate passed.
#[test]
fn the_namespace_constants_agree_across_crates() {
    assert_eq!(evm::NAMESPACE_EIP155, types::NAMESPACE_EIP155);
    assert_eq!(evm::NAMESPACE_TRON, types::NAMESPACE_TRON);
}

/// `POST /wallets` itself returns those addresses - the handler, not the
/// helper underneath it.
///
/// A unit test on `derive_entries` proves the derivation; it does not prove
/// that anything a merchant can reach calls it. This drives the endpoint.
#[tokio::test]
#[ignore]
async fn the_create_wallet_endpoint_returns_addresses_to_verify_a_tron_key() {
    let service = pg_service().await;
    let pool = service.pool().clone();
    let user_id = seed_handler_test_user(&pool).await;
    let state = handler_test_state(service);

    // Unique per run: an xpub may only belong to one account, and this test
    // shares a database with every other.
    let (status, Json(created)) = create_wallet(
        admin_user(user_id),
        State(state),
        Json(CreateWalletRequest {
            xpub: TRON_ACCOUNT_XPUB.to_string(),
            name: Some("tron till".to_string()),
            namespace: "tron".to_string(),
        }),
    )
    .await
    .expect("register a tron wallet");

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(created.wallet.namespace, "tron");
    assert_eq!(
        created
            .verification_addresses
            .iter()
            .map(|e| e.address.clone())
            .collect::<Vec<_>>(),
        vec![
            "TUEZSdKsoDHQMeZwihtdoBiN46zxhGWYdH".to_string(),
            "TSeJkUh4Qv67VNFwY8LaAxERygNdy6NQZK".to_string(),
            "TYJPRrdB5APNeRs4R7fYZSwW3TcrTKw2gx".to_string(),
        ],
        "a merchant has nothing else to check the key with"
    );
}

// =========================================================================
// store_for_caller: foreign and nonexistent stores answer alike
// =========================================================================

fn a_store(owner: UserId) -> Store {
    Store {
        id: StoreId(Uuid::from_bytes([7; 16])),
        name: "s".to_string(),
        website: None,
        owner_id: owner,
        archived: false,
        created_at: Utc::now(),
    }
}

#[test]
fn a_foreign_store_answers_the_same_as_a_nonexistent_one() {
    let me = UserId(Uuid::from_bytes([1; 16]));
    let someone_else = UserId(Uuid::from_bytes([2; 16]));
    for owner_only in [false, true] {
        let foreign = store_for_caller(Some(a_store(someone_else)), me, false, owner_only);
        let missing = store_for_caller(None, me, false, owner_only);
        assert_eq!(foreign.unwrap_err(), StatusCode::NOT_FOUND);
        assert_eq!(missing.unwrap_err(), StatusCode::NOT_FOUND);
    }
}

#[test]
fn a_member_who_is_not_owner_may_read_but_not_manage() {
    let me = UserId(Uuid::from_bytes([1; 16]));
    let owner = UserId(Uuid::from_bytes([2; 16]));
    assert!(store_for_caller(Some(a_store(owner)), me, true, false).is_ok());
    assert_eq!(
        store_for_caller(Some(a_store(owner)), me, true, true).unwrap_err(),
        StatusCode::FORBIDDEN
    );
    assert!(store_for_caller(Some(a_store(me)), me, false, true).is_ok());
}
