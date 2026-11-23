#![allow(clippy::unwrap_used, clippy::expect_used)]

//! The store-settings handlers for webhooks, the token policy, general
//! settings and payment methods all sit behind the shared store-settings
//! permission gate. Between them they decide where payment notifications go,
//! which assets a store accepts, and which key each method derives from - so
//! a narrowed API key that slipped past the gate on any one of them could
//! quietly reconfigure a store it was never meant to touch.
//!
//! Each handler is driven with a real `Some(scope)` in both directions. A
//! refusal alone cannot tell a working guard from a handler that refuses
//! everything, and a grant alone cannot tell a guard from none. Every
//! admitted call also checks the effect landed, and every refusal checks that
//! nothing did - a handler that returned 403 after doing the work would pass
//! a status-only assertion.
//!
//! The scope is written inline in each test, not behind a helper: the scope
//! guard looks for a literal `Some(vec![..])` in the test body.
//!
//! Reads and writes used only to seed or inspect state live in helpers rather
//! than in the test bodies: the scope guard matches handlers by bare name, so
//! a test that merely called a repository method sharing a handler's name
//! would count as driving that handler.

use data_service::test_support::pg_service;
use std::sync::Arc;

use async_trait::async_trait;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use sqlx::PgPool;
use uuid::Uuid;

use api_types::{
    ConfigureWebhookRequest, CreatePaymentMethodRequest, SetTokenPolicyRequest,
    TokenPolicyEntryPayload, UpdatePaymentMethodRequest, UpdateStoreSettingsRequest,
};
use auth::{
    Policies, Result as AuthResult, Role, Session, SessionId, SessionService, Store, UserId,
    UserInfo,
};
use data_service::store_creation::StoreCreationWriter;
use data_service::{PgDataService, StoreTokenPolicyWriter, TokenPolicyMode};
use evm::{ChainFamily, HdWallet, generate_mnemonic};
use rates::NoOpRateProvider;
use server::api::StoreScopedUser;
use server::api::stores::{
    configure_store_webhook, create_payment_method, delete_payment_method, delete_store_webhook,
    delete_token_policy, get_payment_method, get_store_webhook, get_token_policy,
    list_payment_methods, rotate_store_webhook_secret, set_token_policy, update_payment_method,
    update_store_settings,
};
use server::state::PgAppState;
use types::{
    ChainId, StorePaymentMethodReader, StorePaymentMethodWriter, StoreWebhookReader,
    StoreWebhookWriter,
};

struct UnusedSessionService;

#[async_trait]
impl SessionService for UnusedSessionService {
    async fn validate_session(&self, _session_id: SessionId) -> AuthResult<(UserInfo, Session)> {
        unimplemented!("not exercised by these handlers")
    }
    async fn logout(&self, _session_id: SessionId) -> AuthResult<()> {
        unimplemented!("not exercised by these handlers")
    }
    async fn logout_all(&self, _session_id: SessionId) -> AuthResult<()> {
        unimplemented!("not exercised by these handlers")
    }
    async fn cleanup_stale_sessions(&self) -> AuthResult<u64> {
        unimplemented!("not exercised by these handlers")
    }
}

async fn seed_user(pool: &PgPool) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, kdf_params, encrypted_symmetric_key, \
         recovery_verification_hash, kdf_salt_identifier) \
         VALUES ($1, '{}'::jsonb, '{}'::jsonb, 'h', 'passkey:' || $1::text)",
    )
    .bind(id)
    .execute(pool)
    .await
    .expect("seed user");
    id
}

fn user_info(id: Uuid) -> UserInfo {
    UserInfo {
        id: UserId(id),
        email: None,
        primary_wallet_address: None,
        created_at: chrono::Utc::now(),
        last_login_at: None,
        role: Role::User,
    }
}

fn app_state(data_service: Arc<PgDataService>) -> PgAppState<UnusedSessionService> {
    PgAppState::new(
        data_service,
        Arc::new(UnusedSessionService),
        None,
        Arc::new(NoOpRateProvider),
        Arc::new(server::services::email::NoopEmailSender),
    )
}

/// A valid, never-before-registered xpub: a fixed literal would be refused as
/// already claimed on the second run against the same database.
fn fresh_xpub() -> String {
    let mnemonic = generate_mnemonic(24).expect("generate mnemonic");
    HdWallet::from_mnemonic(&mnemonic, "")
        .expect("derive wallet from mnemonic")
        .account_xpub_string_for(ChainFamily::Evm)
        .expect("account xpub")
}

async fn seed_store(pg: &PgDataService, owner: Uuid) -> Store {
    let store = Store::new(format!("store-{}", Uuid::new_v4()), UserId(owner));
    pg.create_store_owned_by(&store, UserId(owner))
        .await
        .expect("seed store owned by user, with the Owner role's canmodifystoresettings");
    store
}

async fn env() -> (PgAppState<UnusedSessionService>, Uuid, Store) {
    let pg = pg_service().await;
    let owner = seed_user(pg.pool()).await;
    let store = seed_store(&pg, owner).await;
    (app_state(Arc::new(pg)), owner, store)
}

// --- seeding and inspection helpers -----------------------------------

async fn seed_webhook(pg: &PgDataService, store_id: Uuid) {
    StoreWebhookWriter::upsert_webhook(pg, store_id, "https://example.com/seeded", "secret", true)
        .await
        .expect("seed webhook");
}

async fn webhook_url(pg: &PgDataService, store_id: Uuid) -> Option<String> {
    StoreWebhookReader::get_webhook(pg, store_id)
        .await
        .expect("read webhook")
        .map(|w| w.webhook_url)
}

async fn webhook_secret(pg: &PgDataService, store_id: Uuid) -> Option<String> {
    StoreWebhookReader::get_webhook(pg, store_id)
        .await
        .expect("read webhook")
        .map(|w| w.webhook_secret)
}

async fn seed_policy(pg: &PgDataService, store_id: Uuid) {
    let entry = data_service::TokenPolicyEntryInput {
        chain_id: ChainId::evm(1),
        token_address: None,
        asset_symbol: "ETH".to_string(),
    };
    StoreTokenPolicyWriter::upsert_token_policy(pg, store_id, TokenPolicyMode::Allowlist, &[entry])
        .await
        .expect("seed token policy");
}

/// The stored policy's mode, read straight from the table.
async fn policy_mode(pg: &PgDataService, store_id: Uuid) -> Option<String> {
    sqlx::query_scalar("SELECT mode FROM store_token_policies WHERE store_id = $1")
        .bind(store_id)
        .fetch_optional(pg.pool())
        .await
        .expect("read token policy")
}

async fn seed_method(pg: &PgDataService, store_id: Uuid) -> Uuid {
    StorePaymentMethodWriter::create_payment_method(
        pg,
        store_id,
        &ChainId::evm(1),
        None,
        "ETH",
        18,
        Some(&fresh_xpub()),
    )
    .await
    .expect("seed payment method")
    .id
}

/// `Some(enabled)` for a method that exists, `None` once it is gone.
async fn method_enabled(pg: &PgDataService, store_id: Uuid, method_id: Uuid) -> Option<bool> {
    StorePaymentMethodReader::get_payment_methods(pg, store_id)
        .await
        .expect("list methods")
        .into_iter()
        .find(|m| m.id == method_id)
        .map(|m| m.enabled)
}

async fn method_count(pg: &PgDataService, store_id: Uuid) -> usize {
    StorePaymentMethodReader::get_payment_methods(pg, store_id)
        .await
        .expect("list methods")
        .len()
}

async fn stored_logo(pg: &PgDataService, store_id: Uuid) -> Option<String> {
    sqlx::query_scalar("SELECT logo_url FROM store_settings WHERE store_id = $1")
        .bind(store_id)
        .fetch_optional(pg.pool())
        .await
        .expect("read store settings")
        .flatten()
}

fn policy_request() -> Json<SetTokenPolicyRequest> {
    Json(SetTokenPolicyRequest {
        mode: "blocklist".to_string(),
        entries: vec![TokenPolicyEntryPayload {
            chain_id: ChainId::evm(1),
            token_address: None,
            asset_symbol: "ETH".to_string(),
        }],
    })
}

fn webhook_request() -> Json<ConfigureWebhookRequest> {
    Json(ConfigureWebhookRequest {
        webhook_url: "https://example.com/hook".to_string(),
        enabled: true,
        rotate_secret: false,
    })
}

// --- webhooks ---------------------------------------------------------

#[tokio::test]
#[ignore]
async fn a_key_scoped_to_modify_settings_can_configure_the_store_webhook() {
    let (state, owner, store) = env().await;
    let result = configure_store_webhook(
        StoreScopedUser(
            user_info(owner),
            Some(vec![Policies::STORE_MODIFY_SETTINGS.to_string()]),
        ),
        State(state.clone()),
        Path(store.id.0),
        webhook_request(),
    )
    .await;
    assert!(result.is_ok(), "must be admitted: {:?}", result.err());
    assert_eq!(
        webhook_url(&state.data_service, store.id.0)
            .await
            .as_deref(),
        Some("https://example.com/hook"),
        "the admitted request must actually store the webhook"
    );
}

/// Where payment notifications go is where an attacker would redirect them.
#[tokio::test]
#[ignore]
async fn a_key_scoped_to_create_invoice_is_refused_configuring_the_store_webhook() {
    let (state, owner, store) = env().await;
    let result = configure_store_webhook(
        StoreScopedUser(
            user_info(owner),
            Some(vec![Policies::STORE_CREATE_INVOICE.to_string()]),
        ),
        State(state.clone()),
        Path(store.id.0),
        webhook_request(),
    )
    .await;
    assert_eq!(result.err(), Some(StatusCode::FORBIDDEN));
    assert_eq!(webhook_url(&state.data_service, store.id.0).await, None);
}

#[tokio::test]
#[ignore]
async fn a_key_scoped_to_modify_settings_can_rotate_the_store_webhook_secret() {
    let (state, owner, store) = env().await;
    seed_webhook(&state.data_service, store.id.0).await;
    let result = rotate_store_webhook_secret(
        StoreScopedUser(
            user_info(owner),
            Some(vec![Policies::STORE_MODIFY_SETTINGS.to_string()]),
        ),
        State(state),
        Path(store.id.0),
    )
    .await;
    let Ok(Json(body)) = result else {
        panic!("a key scoped to canmodifystoresettings must be admitted");
    };
    assert!(body.rotate_secret, "the rotate endpoint always rotates");
    assert!(
        body.webhook_secret.is_some(),
        "the rotated secret must be returned"
    );
}

/// A new secret going to the wrong caller is as bad as redirecting payment
/// notifications - both hand an attacker what they need to forge a webhook.
#[tokio::test]
#[ignore]
async fn a_key_scoped_to_create_invoice_is_refused_rotating_the_store_webhook_secret() {
    let (state, owner, store) = env().await;
    seed_webhook(&state.data_service, store.id.0).await;
    let result = rotate_store_webhook_secret(
        StoreScopedUser(
            user_info(owner),
            Some(vec![Policies::STORE_CREATE_INVOICE.to_string()]),
        ),
        State(state.clone()),
        Path(store.id.0),
    )
    .await;
    assert_eq!(result.err(), Some(StatusCode::FORBIDDEN));
    assert_eq!(
        webhook_secret(&state.data_service, store.id.0)
            .await
            .as_deref(),
        Some("secret"),
        "a refused rotation must not change the existing secret"
    );
}

#[tokio::test]
#[ignore]
async fn a_key_scoped_to_modify_settings_can_get_the_store_webhook() {
    let (state, owner, store) = env().await;
    seed_webhook(&state.data_service, store.id.0).await;
    let result = get_store_webhook(
        StoreScopedUser(
            user_info(owner),
            Some(vec![Policies::STORE_MODIFY_SETTINGS.to_string()]),
        ),
        State(state),
        Path(store.id.0),
    )
    .await;
    let Ok(Json(body)) = result else {
        panic!("a key scoped to canmodifystoresettings must be admitted");
    };
    assert_eq!(body.webhook_url, "https://example.com/seeded");
    assert!(
        body.webhook_secret.is_none(),
        "GET never exposes the secret"
    );
}

/// The webhook exists, so a 404 here would mean the guard was skipped.
#[tokio::test]
#[ignore]
async fn a_key_scoped_to_create_invoice_is_refused_getting_the_store_webhook() {
    let (state, owner, store) = env().await;
    seed_webhook(&state.data_service, store.id.0).await;
    let result = get_store_webhook(
        StoreScopedUser(
            user_info(owner),
            Some(vec![Policies::STORE_CREATE_INVOICE.to_string()]),
        ),
        State(state),
        Path(store.id.0),
    )
    .await;
    assert_eq!(result.err(), Some(StatusCode::FORBIDDEN));
}

#[tokio::test]
#[ignore]
async fn a_key_scoped_to_modify_settings_can_delete_the_store_webhook() {
    let (state, owner, store) = env().await;
    seed_webhook(&state.data_service, store.id.0).await;
    let result = delete_store_webhook(
        StoreScopedUser(
            user_info(owner),
            Some(vec![Policies::STORE_MODIFY_SETTINGS.to_string()]),
        ),
        State(state.clone()),
        Path(store.id.0),
    )
    .await;
    assert_eq!(result.ok(), Some(StatusCode::NO_CONTENT));
    assert_eq!(webhook_url(&state.data_service, store.id.0).await, None);
}

#[tokio::test]
#[ignore]
async fn a_key_scoped_to_create_invoice_is_refused_deleting_the_store_webhook() {
    let (state, owner, store) = env().await;
    seed_webhook(&state.data_service, store.id.0).await;
    let result = delete_store_webhook(
        StoreScopedUser(
            user_info(owner),
            Some(vec![Policies::STORE_CREATE_INVOICE.to_string()]),
        ),
        State(state.clone()),
        Path(store.id.0),
    )
    .await;
    assert_eq!(result.err(), Some(StatusCode::FORBIDDEN));
    assert!(
        webhook_url(&state.data_service, store.id.0).await.is_some(),
        "a refused delete must leave the webhook in place"
    );
}

// --- token policy -----------------------------------------------------

#[tokio::test]
#[ignore]
async fn a_key_scoped_to_modify_settings_can_set_the_token_policy() {
    let (state, owner, store) = env().await;
    let result = set_token_policy(
        StoreScopedUser(
            user_info(owner),
            Some(vec![Policies::STORE_MODIFY_SETTINGS.to_string()]),
        ),
        State(state.clone()),
        Path(store.id.0),
        policy_request(),
    )
    .await;
    assert!(result.is_ok(), "must be admitted: {:?}", result.err());
    assert_eq!(
        policy_mode(&state.data_service, store.id.0)
            .await
            .as_deref(),
        Some("blocklist"),
        "the admitted request must actually store the policy"
    );
}

#[tokio::test]
#[ignore]
async fn a_key_scoped_to_create_invoice_is_refused_setting_the_token_policy() {
    let (state, owner, store) = env().await;
    let result = set_token_policy(
        StoreScopedUser(
            user_info(owner),
            Some(vec![Policies::STORE_CREATE_INVOICE.to_string()]),
        ),
        State(state.clone()),
        Path(store.id.0),
        policy_request(),
    )
    .await;
    assert_eq!(result.err(), Some(StatusCode::FORBIDDEN));
    assert_eq!(policy_mode(&state.data_service, store.id.0).await, None);
}

#[tokio::test]
#[ignore]
async fn a_key_scoped_to_modify_settings_can_get_the_token_policy() {
    let (state, owner, store) = env().await;
    seed_policy(&state.data_service, store.id.0).await;
    let result = get_token_policy(
        StoreScopedUser(
            user_info(owner),
            Some(vec![Policies::STORE_MODIFY_SETTINGS.to_string()]),
        ),
        State(state),
        Path(store.id.0),
    )
    .await;
    let Ok(Json(Some(policy))) = result else {
        panic!("a key scoped to canmodifystoresettings must be admitted and see the policy");
    };
    assert_eq!(policy.mode, "allowlist");
}

#[tokio::test]
#[ignore]
async fn a_key_scoped_to_create_invoice_is_refused_getting_the_token_policy() {
    let (state, owner, store) = env().await;
    seed_policy(&state.data_service, store.id.0).await;
    let result = get_token_policy(
        StoreScopedUser(
            user_info(owner),
            Some(vec![Policies::STORE_CREATE_INVOICE.to_string()]),
        ),
        State(state),
        Path(store.id.0),
    )
    .await;
    assert_eq!(result.err(), Some(StatusCode::FORBIDDEN));
}

#[tokio::test]
#[ignore]
async fn a_key_scoped_to_modify_settings_can_delete_the_token_policy() {
    let (state, owner, store) = env().await;
    seed_policy(&state.data_service, store.id.0).await;
    let result = delete_token_policy(
        StoreScopedUser(
            user_info(owner),
            Some(vec![Policies::STORE_MODIFY_SETTINGS.to_string()]),
        ),
        State(state.clone()),
        Path(store.id.0),
    )
    .await;
    assert_eq!(result.ok(), Some(StatusCode::NO_CONTENT));
    assert_eq!(policy_mode(&state.data_service, store.id.0).await, None);
}

/// Deleting the policy reverts the store to accept-all, so a key that could
/// do it silently widens what the store takes.
#[tokio::test]
#[ignore]
async fn a_key_scoped_to_create_invoice_is_refused_deleting_the_token_policy() {
    let (state, owner, store) = env().await;
    seed_policy(&state.data_service, store.id.0).await;
    let result = delete_token_policy(
        StoreScopedUser(
            user_info(owner),
            Some(vec![Policies::STORE_CREATE_INVOICE.to_string()]),
        ),
        State(state.clone()),
        Path(store.id.0),
    )
    .await;
    assert_eq!(result.err(), Some(StatusCode::FORBIDDEN));
    assert_eq!(
        policy_mode(&state.data_service, store.id.0)
            .await
            .as_deref(),
        Some("allowlist"),
        "a refused delete must leave the policy in place"
    );
}

// --- store settings ---------------------------------------------------

fn settings_request() -> Json<UpdateStoreSettingsRequest> {
    Json(UpdateStoreSettingsRequest {
        default_chain_id: None,
        default_display_currency: None,
        logo_url: Some("https://example.com/logo.png".to_string()),
        accent_color: None,
        notification_prefs: None,
    })
}

#[tokio::test]
#[ignore]
async fn a_key_scoped_to_modify_settings_can_update_store_settings() {
    let (state, owner, store) = env().await;
    let result = update_store_settings(
        StoreScopedUser(
            user_info(owner),
            Some(vec![Policies::STORE_MODIFY_SETTINGS.to_string()]),
        ),
        State(state.clone()),
        Path(store.id.0),
        settings_request(),
    )
    .await;
    assert!(result.is_ok(), "must be admitted: {:?}", result.err());
    assert_eq!(
        stored_logo(&state.data_service, store.id.0)
            .await
            .as_deref(),
        Some("https://example.com/logo.png"),
        "the admitted request must actually store the setting"
    );
}

#[tokio::test]
#[ignore]
async fn a_key_scoped_to_create_invoice_is_refused_updating_store_settings() {
    let (state, owner, store) = env().await;
    let result = update_store_settings(
        StoreScopedUser(
            user_info(owner),
            Some(vec![Policies::STORE_CREATE_INVOICE.to_string()]),
        ),
        State(state.clone()),
        Path(store.id.0),
        settings_request(),
    )
    .await;
    assert_eq!(result.err(), Some(StatusCode::FORBIDDEN));
    assert_eq!(stored_logo(&state.data_service, store.id.0).await, None);
}

// --- payment methods --------------------------------------------------

/// A testnet chain: with no server-settings row - a fresh database, as in CI -
/// creation only accepts chains that have a testnet configuration.
fn create_method_request() -> Json<CreatePaymentMethodRequest> {
    Json(CreatePaymentMethodRequest {
        chain_id: ChainId::evm(11_155_111),
        token_address: None,
        asset_symbol: "ETH".to_string(),
        decimals: 18,
        xpub: Some(fresh_xpub()),
    })
}

#[tokio::test]
#[ignore]
async fn a_key_scoped_to_modify_settings_can_create_a_payment_method() {
    let (state, owner, store) = env().await;
    let result = create_payment_method(
        StoreScopedUser(
            user_info(owner),
            Some(vec![Policies::STORE_MODIFY_SETTINGS.to_string()]),
        ),
        State(state.clone()),
        Path(store.id.0),
        create_method_request(),
    )
    .await;
    let Ok((status, _)) = result else {
        panic!("a key scoped to canmodifystoresettings must be admitted");
    };
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(method_count(&state.data_service, store.id.0).await, 1);
}

/// A method carries the xpub payments derive from, so creating one is
/// choosing a destination.
#[tokio::test]
#[ignore]
async fn a_key_scoped_to_create_invoice_is_refused_creating_a_payment_method() {
    let (state, owner, store) = env().await;
    let result = create_payment_method(
        StoreScopedUser(
            user_info(owner),
            Some(vec![Policies::STORE_CREATE_INVOICE.to_string()]),
        ),
        State(state.clone()),
        Path(store.id.0),
        create_method_request(),
    )
    .await;
    let Err(err) = result else {
        panic!("a key not scoped to canmodifystoresettings must be refused");
    };
    assert_eq!(err.into_response().status(), StatusCode::FORBIDDEN);
    assert_eq!(method_count(&state.data_service, store.id.0).await, 0);
}

#[tokio::test]
#[ignore]
async fn a_key_scoped_to_modify_settings_can_list_payment_methods() {
    let (state, owner, store) = env().await;
    let method = seed_method(&state.data_service, store.id.0).await;
    let result = list_payment_methods(
        StoreScopedUser(
            user_info(owner),
            Some(vec![Policies::STORE_MODIFY_SETTINGS.to_string()]),
        ),
        State(state),
        Path(store.id.0),
    )
    .await;
    let Ok(Json(methods)) = result else {
        panic!("a key scoped to canmodifystoresettings must be admitted");
    };
    assert_eq!(methods.len(), 1);
    assert_eq!(methods[0].id, method);
}

#[tokio::test]
#[ignore]
async fn a_key_scoped_to_create_invoice_is_refused_listing_payment_methods() {
    let (state, owner, store) = env().await;
    seed_method(&state.data_service, store.id.0).await;
    let result = list_payment_methods(
        StoreScopedUser(
            user_info(owner),
            Some(vec![Policies::STORE_CREATE_INVOICE.to_string()]),
        ),
        State(state),
        Path(store.id.0),
    )
    .await;
    assert_eq!(result.err(), Some(StatusCode::FORBIDDEN));
}

#[tokio::test]
#[ignore]
async fn a_key_scoped_to_modify_settings_can_get_a_payment_method() {
    let (state, owner, store) = env().await;
    let method = seed_method(&state.data_service, store.id.0).await;
    let result = get_payment_method(
        StoreScopedUser(
            user_info(owner),
            Some(vec![Policies::STORE_MODIFY_SETTINGS.to_string()]),
        ),
        State(state),
        Path((store.id.0, method)),
    )
    .await;
    let Ok(Json(body)) = result else {
        panic!("a key scoped to canmodifystoresettings must be admitted");
    };
    assert_eq!(body.id, method);
}

/// The method exists, so a 404 here would mean the guard was skipped.
#[tokio::test]
#[ignore]
async fn a_key_scoped_to_create_invoice_is_refused_getting_a_payment_method() {
    let (state, owner, store) = env().await;
    let method = seed_method(&state.data_service, store.id.0).await;
    let result = get_payment_method(
        StoreScopedUser(
            user_info(owner),
            Some(vec![Policies::STORE_CREATE_INVOICE.to_string()]),
        ),
        State(state),
        Path((store.id.0, method)),
    )
    .await;
    assert_eq!(result.err(), Some(StatusCode::FORBIDDEN));
}

#[tokio::test]
#[ignore]
async fn a_key_scoped_to_modify_settings_can_update_a_payment_method() {
    let (state, owner, store) = env().await;
    let method = seed_method(&state.data_service, store.id.0).await;
    let result = update_payment_method(
        StoreScopedUser(
            user_info(owner),
            Some(vec![Policies::STORE_MODIFY_SETTINGS.to_string()]),
        ),
        State(state.clone()),
        Path((store.id.0, method)),
        Json(UpdatePaymentMethodRequest {
            enabled: Some(false),
            xpub: None,
        }),
    )
    .await;
    assert!(result.is_ok(), "must be admitted: {:?}", result.err());
    assert_eq!(
        method_enabled(&state.data_service, store.id.0, method).await,
        Some(false),
        "the admitted request must actually disable the method"
    );
}

#[tokio::test]
#[ignore]
async fn a_key_scoped_to_create_invoice_is_refused_updating_a_payment_method() {
    let (state, owner, store) = env().await;
    let method = seed_method(&state.data_service, store.id.0).await;
    let result = update_payment_method(
        StoreScopedUser(
            user_info(owner),
            Some(vec![Policies::STORE_CREATE_INVOICE.to_string()]),
        ),
        State(state.clone()),
        Path((store.id.0, method)),
        Json(UpdatePaymentMethodRequest {
            enabled: Some(false),
            xpub: None,
        }),
    )
    .await;
    let Err(err) = result else {
        panic!("a key not scoped to canmodifystoresettings must be refused");
    };
    assert_eq!(err.into_response().status(), StatusCode::FORBIDDEN);
    assert_eq!(
        method_enabled(&state.data_service, store.id.0, method).await,
        Some(true),
        "a refused update must leave the method as it was"
    );
}

#[tokio::test]
#[ignore]
async fn a_key_scoped_to_modify_settings_can_delete_a_payment_method() {
    let (state, owner, store) = env().await;
    let method = seed_method(&state.data_service, store.id.0).await;
    let result = delete_payment_method(
        StoreScopedUser(
            user_info(owner),
            Some(vec![Policies::STORE_MODIFY_SETTINGS.to_string()]),
        ),
        State(state.clone()),
        Path((store.id.0, method)),
    )
    .await;
    assert_eq!(result.ok(), Some(StatusCode::NO_CONTENT));
    assert_eq!(
        method_enabled(&state.data_service, store.id.0, method).await,
        None
    );
}

#[tokio::test]
#[ignore]
async fn a_key_scoped_to_create_invoice_is_refused_deleting_a_payment_method() {
    let (state, owner, store) = env().await;
    let method = seed_method(&state.data_service, store.id.0).await;
    let result = delete_payment_method(
        StoreScopedUser(
            user_info(owner),
            Some(vec![Policies::STORE_CREATE_INVOICE.to_string()]),
        ),
        State(state.clone()),
        Path((store.id.0, method)),
    )
    .await;
    assert_eq!(result.err(), Some(StatusCode::FORBIDDEN));
    assert!(
        method_enabled(&state.data_service, store.id.0, method)
            .await
            .is_some(),
        "a refused delete must leave the method in place"
    );
}
