//! Shared fixtures for the cross-tenant isolation suite: seeding a tenant's
//! full footprint, building an authenticated caller, and the two small
//! helpers every section's assertions go through.

use std::sync::Arc;

use alloy_primitives::U256;
use async_trait::async_trait;
use axum::extract::FromRequestParts;
use axum::http::{Request as HttpRequest, StatusCode};
use axum::response::IntoResponse;
use chrono::Utc;
use sqlx::PgPool;
use uuid::Uuid;

use auth::{
    ApiKey, ApiKeyId, ApiKeyRepository, Result as AuthResult, Session, SessionId, SessionService,
    Store, StoreRole, StoreRoleRepository, UserId, UserInfo, UserStore, UserStoreRepository,
};
use data_service::store_creation::StoreCreationWriter;
use data_service::{
    PayoutData, PayoutStatus, PayoutWriter, PgDataService, RefundData, RefundStatus, RefundWriter,
    StoreWebhookWriter, UpsertDeliveryParams, WalletWriter, WebhookDeliveryStatus,
    WebhookDeliveryWriter,
};
use rates::NoOpRateProvider;
use server::api::{AuthenticatedUser, StoreScopedUser};
use server::services::RedisEVMMonitor;
use server::state::PgAppState;
use types::{
    AssetType, ChainId, InvoiceData, InvoiceId, InvoiceStatus, InvoiceWriter, NAMESPACE_EIP155,
    PaymentData, PaymentWriter,
};

/// Not exercised: every handler under test reads only the already-
/// authenticated `UserInfo` this file constructs directly, or the API-key
/// path, which never calls back into session management either.
pub(crate) struct UnusedSessionService;

#[async_trait]
impl SessionService for UnusedSessionService {
    async fn validate_session(&self, _session_id: SessionId) -> AuthResult<(UserInfo, Session)> {
        unimplemented!("not exercised by the handlers under test")
    }
    async fn logout(&self, _session_id: SessionId) -> AuthResult<()> {
        unimplemented!("not exercised by the handlers under test")
    }
    async fn logout_all(&self, _session_id: SessionId) -> AuthResult<()> {
        unimplemented!("not exercised by the handlers under test")
    }
    async fn cleanup_stale_sessions(&self) -> AuthResult<u64> {
        unimplemented!("not exercised by the handlers under test")
    }
}

pub(crate) use data_service::test_support::pg_service as service;

pub(crate) async fn seed_user(pool: &PgPool) -> Uuid {
    let id = Uuid::new_v4();
    // `kdf_params`/`encrypted_symmetric_key` need to deserialize into real
    // `crypto::KdfParams`/`EncryptedBlob` shapes (not just be valid JSON) -
    // the API-key auth path resolves the owning user via `UserRepository::
    // get_user`, which does that deserialization, unlike every other test
    // in this crate that hands `AuthenticatedUser` a `UserInfo` directly and
    // never reads this row back.
    sqlx::query(
        "INSERT INTO users (id, kdf_params, encrypted_symmetric_key, \
         recovery_verification_hash, kdf_salt_identifier) \
         VALUES ($1, \
         '{\"algorithm\":\"argon2id\",\"memory_kb\":65536,\"iterations\":3,\"parallelism\":4,\"salt\":\"AAAA\"}'::jsonb, \
         '{\"ciphertext\":\"AAAA\",\"iv\":\"AAAA\",\"mac\":\"AAAA\"}'::jsonb, \
         'h', 'passkey:' || $1::text)",
    )
    .bind(id)
    .execute(pool)
    .await
    .expect("seed user");
    id
}

pub(crate) fn user_info(id: Uuid) -> UserInfo {
    user_info_with_role(id, auth::Role::User)
}

pub(crate) fn user_info_with_role(id: Uuid, role: auth::Role) -> UserInfo {
    UserInfo {
        id: UserId(id),
        email: None,
        primary_wallet_address: None,
        created_at: Utc::now(),
        last_login_at: None,
        role,
    }
}

pub(crate) fn app_state(data_service: Arc<PgDataService>) -> PgAppState<UnusedSessionService> {
    PgAppState::new(
        data_service,
        Arc::new(UnusedSessionService),
        None::<Arc<RedisEVMMonitor>>,
        Arc::new(NoOpRateProvider),
        Arc::new(server::services::email::NoopEmailSender),
    )
}

pub(crate) fn sha256_hex(input: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    hex::encode(hasher.finalize())
}

/// One merchant's full footprint: a store, an invoice on it, a payment on
/// that invoice, an account wallet, and an API key.
pub(crate) struct Tenant {
    pub(crate) user_id: Uuid,
    pub(crate) store: Store,
    pub(crate) invoice: InvoiceData,
    pub(crate) payment_id: Uuid,
    pub(crate) wallet: data_service::Wallet,
    pub(crate) api_key_raw: String,
}

pub(crate) async fn seed_tenant(pg: &PgDataService, label: &str) -> Tenant {
    let user_id = seed_user(pg.pool()).await;
    let store = Store::new(format!("store-{label}-{}", Uuid::new_v4()), UserId(user_id));
    pg.create_store_owned_by(&store, UserId(user_id))
        .await
        .expect("seed store owned by user");

    let invoice = InvoiceData {
        id: InvoiceId::new(),
        store_id: types::StoreId(store.id.0),
        currency: "USD".to_string(),
        status: InvoiceStatus::Pending,
        amount: "100.00".to_string(),
        amount_received: "0".to_string(),
        created_at: Utc::now(),
        expires_at: Utc::now() + chrono::Duration::hours(1),
        metadata: None,
        customer_email: None,
        extra: None,
    };
    InvoiceWriter::upsert(pg, &invoice)
        .await
        .expect("seed invoice");

    let payment = PaymentData {
        id: Uuid::new_v4(),
        invoice_id: invoice.id.clone(),
        payment_option_id: None,
        chain_id: ChainId::evm(11155111),
        asset_type: AssetType::Native,
        amount: "1000000000000000000".to_string(),
        asset_symbol: "ETH".to_string(),
        token_address: None,
        tx_hash: format!("0x{:064x}", Uuid::new_v4().as_u128()),
        block_number: Some(1),
        detected_at: Utc::now(),
        confirmed_at: None,
        from_address: None,
        reorged: false,
        extra: None,
        credited_amount: None,
        rate_used: None,
        rate_applied_at: None,
    };
    PaymentWriter::upsert(pg, &payment)
        .await
        .expect("seed payment");

    let wallet = WalletWriter::create_wallet(
        pg,
        user_id,
        NAMESPACE_EIP155,
        &format!("xpub-fake-{}", Uuid::new_v4()),
        None,
    )
    .await
    .expect("seed wallet");

    let api_key_raw = format!("ak_test_{}", Uuid::new_v4());
    ApiKeyRepository::create_api_key(
        pg,
        &ApiKey {
            id: ApiKeyId::new(),
            user_id: UserId(user_id),
            name: "cross-tenant test key".to_string(),
            key_hash: sha256_hex(&api_key_raw),
            key_prefix: "ak_test_****".to_string(),
            is_active: true,
            created_at: Utc::now(),
            last_used_at: None,
            expires_at: None,
        },
    )
    .await
    .expect("seed api key");

    Tenant {
        user_id,
        store,
        invoice,
        payment_id: payment.id,
        wallet,
        api_key_raw,
    }
}

/// Switches a tenant's role on their own store to a one-off role carrying
/// exactly `permission`, for the one endpoint (`list_store_members`) whose
/// permission string none of the seeded default roles - Owner included -
/// actually grant. Without this, a positive control against that endpoint
/// would fail for every caller, not just a foreign one, which is a real gap
/// in this repo's default roles but not what this suite exists to prove.
pub(crate) async fn grant_store_permission(pg: &PgDataService, tenant: &Tenant, permission: &str) {
    let role = StoreRole::new(
        auth::StoreId(tenant.store.id.0),
        "cross-tenant-test-role",
        vec![permission.to_string()],
    );
    StoreRoleRepository::create_store_role(pg, &role)
        .await
        .expect("create a role carrying the permission under test");
    UserStoreRepository::update_user_store(
        pg,
        &UserStore::new(
            UserId(tenant.user_id),
            auth::StoreId(tenant.store.id.0),
            role.id,
        ),
    )
    .await
    .expect("assign the test role to the tenant's own store membership");
}

/// A confirmed payment on `tenant`'s own invoice, on the default chain/asset
/// `create_payout` sweeps (`eip155:11155111`, native ETH) - unlike
/// `seed_tenant`'s own payment (`confirmed_at: None`, standing in for "not
/// yet payable"), this one is what `sum_payable` actually counts: confirmed,
/// not reorged, matching chain and asset. Returns the payment's base-unit
/// amount as a `U256` so a caller can assert the payout total it produces.
pub(crate) async fn seed_confirmed_payment(pg: &PgDataService, tenant: &Tenant) -> U256 {
    const AMOUNT: &str = "500000000000000000";
    let payment = PaymentData {
        id: Uuid::new_v4(),
        invoice_id: tenant.invoice.id.clone(),
        payment_option_id: None,
        chain_id: ChainId::evm(11155111),
        asset_type: AssetType::Native,
        amount: AMOUNT.to_string(),
        asset_symbol: "ETH".to_string(),
        token_address: None,
        tx_hash: format!("0x{:064x}", Uuid::new_v4().as_u128()),
        block_number: Some(1),
        detected_at: Utc::now(),
        confirmed_at: Some(Utc::now()),
        from_address: None,
        reorged: false,
        extra: None,
        credited_amount: None,
        rate_used: None,
        rate_applied_at: None,
    };
    PaymentWriter::upsert(pg, &payment)
        .await
        .expect("seed confirmed payment");
    AMOUNT.parse().expect("AMOUNT is a valid U256")
}

/// A payout on `store`, unrelated to any real invoice - the payout endpoints
/// under test only ever check the payout's own `store_id`, never its
/// `invoice_ids`.
pub(crate) async fn seed_payout(pg: &PgDataService, store: &Store) -> Uuid {
    let payout = PayoutData {
        id: Uuid::new_v4(),
        store_id: types::StoreId(store.id.0),
        invoice_ids: vec![],
        destination_address: format!("0x{:040x}", Uuid::new_v4().as_u128()),
        chain_id: ChainId::evm(11155111),
        asset_type: "native".to_string(),
        asset_symbol: "ETH".to_string(),
        token_address: None,
        amount: "1000000000000000000".to_string(),
        tx_hash: None,
        status: PayoutStatus::Pending,
        fee_amount: None,
        error_message: None,
        created_at: Utc::now(),
        confirmed_at: None,
    };
    PayoutWriter::create_payout(pg, &payout)
        .await
        .expect("seed payout");
    payout.id
}

/// A refund on the tenant's own invoice and payment.
pub(crate) async fn seed_refund(pg: &PgDataService, tenant: &Tenant) -> Uuid {
    let refund = RefundData {
        id: Uuid::new_v4(),
        invoice_id: tenant.invoice.id.clone(),
        payment_id: tenant.payment_id,
        store_id: types::StoreId(tenant.store.id.0),
        to_address: format!("0x{:040x}", Uuid::new_v4().as_u128()),
        chain_id: ChainId::evm(11155111),
        asset_type: "native".to_string(),
        asset_symbol: "ETH".to_string(),
        token_address: None,
        amount: "500000000000000000".to_string(),
        tx_hash: None,
        status: RefundStatus::Pending,
        fee_amount: None,
        reason: None,
        error_message: None,
        created_at: Utc::now(),
        confirmed_at: None,
    };
    RefundWriter::create_refund(pg, &refund)
        .await
        .expect("seed refund");
    refund.id
}

/// A delivered webhook delivery against the tenant's own invoice, behind a
/// webhook configured for the tenant's store.
pub(crate) async fn seed_webhook_delivery(pg: &PgDataService, tenant: &Tenant) -> Uuid {
    let webhook = StoreWebhookWriter::upsert_webhook(
        pg,
        tenant.store.id.0,
        "https://example.com/webhook",
        "secret",
        true,
    )
    .await
    .expect("seed store webhook");

    let delivery_id = Uuid::new_v4();
    WebhookDeliveryWriter::upsert_delivery(
        pg,
        UpsertDeliveryParams {
            id: delivery_id,
            store_webhook_id: webhook.id,
            invoice_id: tenant.invoice.id.0.clone(),
            event_type: "invoice_expired".to_string(),
            status: WebhookDeliveryStatus::Delivered,
            attempts: 1,
            max_attempts: 7,
            last_error: None,
            payload: serde_json::json!({"event_type": "invoice_expired"}),
        },
    )
    .await
    .expect("seed webhook delivery");
    delivery_id
}

/// Runs a raw bearer token through the same extractor a real request would,
/// so the API-key branch of `validate_session` (hashing, active/expiry
/// checks, resolving the owning user) is what authenticates - not a
/// hand-built `UserInfo` that would skip it entirely.
pub(crate) async fn authenticate_via_bearer<A: SessionService + 'static>(
    state: &PgAppState<A>,
    raw_token: &str,
) -> AuthenticatedUser {
    let request = HttpRequest::builder()
        .header("authorization", format!("Bearer {raw_token}"))
        .body(())
        .expect("build request");
    let (mut parts, ()) = request.into_parts();
    AuthenticatedUser::from_request_parts(&mut parts, state)
        .await
        .expect("bearer token authenticates")
}

/// Same as `authenticate_via_bearer`, for the handlers under test that take
/// `StoreScopedUser` instead of `AuthenticatedUser` - the ones that gate a
/// store-scoped action and so must see the key's own permission scope, not
/// just who it authenticates as.
pub(crate) async fn authenticate_via_bearer_scoped<A: SessionService + 'static>(
    state: &PgAppState<A>,
    raw_token: &str,
) -> StoreScopedUser {
    let request = HttpRequest::builder()
        .header("authorization", format!("Bearer {raw_token}"))
        .body(())
        .expect("build request");
    let (mut parts, ()) = request.into_parts();
    StoreScopedUser::from_request_parts(&mut parts, state)
        .await
        .expect("bearer token authenticates")
}

/// A plain, unscoped `StoreScopedUser` for a session-style caller in tests
/// that exercise a handler which now requires `StoreScopedUser` - `None`
/// matches a real session's own scope (see `validate_session_with_scope`)
/// and preserves this suite's original "ownership only, no key narrowing"
/// intent for those cases.
pub(crate) fn store_scoped_user_info(id: Uuid) -> StoreScopedUser {
    StoreScopedUser(user_info(id), None)
}

pub(crate) fn status_of<T>(result: Result<T, server::api::ApiErr>) -> StatusCode {
    match result {
        Ok(_) => StatusCode::OK,
        Err(e) => e.into_response().status(),
    }
}
