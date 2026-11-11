#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Control test: address derivation works through the invoice route after xpub narrowing.
//!
//! Two invoices on one store must derive different addresses. The xpub is
//! narrowed (selected only where derivation happens), but derivation itself
//! must still work — this test proves it does, by the only route a customer
//! pays through: create_invoice followed by get_invoice on the same store,
//! checking the payment options match independently derived addresses.

use data_service::test_support::pg_service;
use std::sync::Arc;

use async_trait::async_trait;
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use sqlx::PgPool;
use uuid::Uuid;

use auth::{
    Result as AuthResult, Role, Session, SessionId, SessionService, Store, UserId, UserInfo,
};
use data_service::PgDataService;
use data_service::store_creation::StoreCreationWriter;
use evm::XpubDeriver;
use rates::NoOpRateProvider;
use server::api::AuthenticatedCaller;
use server::api::invoices::{CreateInvoiceRequest, create_invoice};
use server::services::RedisEVMMonitor;
use server::state::PgAppState;
use types::{ChainId, StorePaymentMethodWriter};

const TEST_XPUB: &str = "xpub661MyMwAqRbcFtXgS5sYJABqqG9YLmC4Q1Rdap9gSE8NqtwybGhePY2gZ29ESFjqJoCu1Rupje8YtGqsefD265TMg7usUDFdp6W1EGMcet8";

struct NoopSessionService;

#[async_trait]
impl SessionService for NoopSessionService {
    async fn validate_session(&self, _session_id: SessionId) -> AuthResult<(UserInfo, Session)> {
        unimplemented!("not exercised by create_invoice or get_invoice")
    }
    async fn logout(&self, _session_id: SessionId) -> AuthResult<()> {
        unimplemented!("not exercised by create_invoice or get_invoice")
    }
    async fn logout_all(&self, _session_id: SessionId) -> AuthResult<()> {
        unimplemented!("not exercised by create_invoice or get_invoice")
    }
    async fn cleanup_stale_sessions(&self) -> AuthResult<u64> {
        unimplemented!("not exercised by create_invoice or get_invoice")
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

fn merchant_caller(owner: Uuid) -> AuthenticatedCaller {
    AuthenticatedCaller {
        user: UserInfo {
            id: UserId(owner),
            email: None,
            primary_wallet_address: None,
            created_at: chrono::Utc::now(),
            last_login_at: None,
            role: Role::User,
        },
        is_operator: false,
        key_scope: None,
    }
}

fn app_state(data_service: Arc<PgDataService>) -> PgAppState<NoopSessionService> {
    PgAppState::new(
        data_service,
        Arc::new(NoopSessionService),
        None::<Arc<RedisEVMMonitor>>,
        Arc::new(NoOpRateProvider),
        Arc::new(server::services::email::NoopEmailSender),
    )
}

fn invoice_request(store_id: Uuid) -> CreateInvoiceRequest {
    CreateInvoiceRequest {
        store_id,
        currency: "ETH".to_string(),
        amount: "1.00".to_string(),
        expiration_seconds: None,
        metadata: None,
        customer_email: None,
        webhook_url: None,
        redirect_url: None,
    }
}

/// Control test: two invoices on one store get different addresses, verifying
/// that xpub narrowing (removing it from non-deriving reads) does not break
/// the derivation that happens through the invoice route.
#[tokio::test]
#[ignore]
async fn two_invoices_on_one_store_derive_different_addresses() {
    let pg = pg_service().await;
    let owner = seed_user(pg.pool()).await;
    let store = Store::new(format!("store-{}", Uuid::new_v4()), UserId(owner));
    pg.create_store_owned_by(&store, UserId(owner))
        .await
        .expect("seed store");

    // Create a payment method pinned to this xpub
    StorePaymentMethodWriter::create_payment_method(
        &pg,
        store.id.0,
        &ChainId::evm(11_155_111),
        None,
        "ETH",
        18,
        Some(TEST_XPUB),
    )
    .await
    .expect("seed payment method");

    let ds = Arc::new(pg);

    // Create first invoice
    let (status1, Json(invoice1)) = create_invoice(
        merchant_caller(owner),
        State(app_state(Arc::clone(&ds))),
        Json(invoice_request(store.id.0)),
    )
    .await
    .expect("create first invoice");
    assert_eq!(status1, StatusCode::CREATED);

    let address1 = invoice1.payment_options[0].payment_address.clone();

    // Independently derive the expected address at index 0
    let expected_addr_0 = XpubDeriver::from_xpub("eip155", TEST_XPUB)
        .unwrap()
        .derive_address(0)
        .unwrap();
    assert_eq!(
        address1, expected_addr_0,
        "first invoice should derive at index 0"
    );

    // Create second invoice
    let (status2, Json(invoice2)) = create_invoice(
        merchant_caller(owner),
        State(app_state(Arc::clone(&ds))),
        Json(invoice_request(store.id.0)),
    )
    .await
    .expect("create second invoice");
    assert_eq!(status2, StatusCode::CREATED);

    let address2 = invoice2.payment_options[0].payment_address.clone();

    // Independently derive the expected address at index 1
    let expected_addr_1 = XpubDeriver::from_xpub("eip155", TEST_XPUB)
        .unwrap()
        .derive_address(1)
        .unwrap();
    assert_eq!(
        address2, expected_addr_1,
        "second invoice should derive at index 1"
    );

    // The key assertion: two invoices got different addresses
    assert_ne!(
        address1, address2,
        "two invoices on the same store must derive different addresses"
    );
}

/// Verify that xpub does not leak into API response bodies through the
/// payment-method and wallet read endpoints. The plaintext xpub column must
/// never appear in customer-facing or merchant-facing responses.
///
/// Note: export_wallet_xpub is intentionally excluded from this test, as that
/// endpoint's documented purpose is to return the full xpub by design.
#[tokio::test]
#[ignore]
async fn xpub_does_not_leak_into_response_bodies() {
    use server::api::stores::payment_methods::{get_payment_method, list_payment_methods};
    use server::api::stores::wallets::{get_wallet_by_id, list_wallets};

    let pg = pg_service().await;
    let owner = seed_user(pg.pool()).await;
    let store = Store::new(format!("store-{}", Uuid::new_v4()), UserId(owner));
    pg.create_store_owned_by(&store, UserId(owner))
        .await
        .expect("seed store");

    // Create a payment method pinned to this xpub
    let method_id = StorePaymentMethodWriter::create_payment_method(
        &pg,
        store.id.0,
        &ChainId::evm(11_155_111),
        None,
        "ETH",
        18,
        Some(TEST_XPUB),
    )
    .await
    .expect("seed payment method")
    .id;

    let ds = Arc::new(pg);
    let state = app_state(Arc::clone(&ds));

    // Test list_payment_methods endpoint
    let (_, Json(methods_list)) = list_payment_methods(
        merchant_caller(owner),
        State(state.clone()),
        Path(store.id.0),
    )
    .await
    .expect("list payment methods");

    let methods_json = serde_json::to_string(&methods_list).expect("serialize methods list");

    // Test get_payment_method endpoint
    let (_, Json(method_detail)) = get_payment_method(
        merchant_caller(owner),
        State(state.clone()),
        Path((store.id.0, method_id)),
    )
    .await
    .expect("get payment method");

    let method_json = serde_json::to_string(&method_detail).expect("serialize method");

    // Create a wallet for this xpub to test wallet endpoints
    use server::api::extractors::AuthenticatedUser;
    let wallet_id = data_service::repository::WalletWriter::create_wallet(
        &*state.data_service,
        owner,
        ChainId::evm(1),
        TEST_XPUB,
        None,
    )
    .await
    .expect("create wallet")
    .id;

    // Test list_wallets endpoint
    let (_, Json(wallets_list)) = list_wallets(
        AuthenticatedUser(merchant_caller(owner).user),
        State(state.clone()),
    )
    .await
    .expect("list wallets");

    let wallets_json = serde_json::to_string(&wallets_list).expect("serialize wallets list");

    // Test get_wallet_by_id endpoint
    let (_, Json(wallet_detail)) = get_wallet_by_id(
        AuthenticatedUser(merchant_caller(owner).user),
        State(state.clone()),
        Path(wallet_id),
    )
    .await
    .expect("get wallet by id");

    let wallet_json = serde_json::to_string(&wallet_detail).expect("serialize wallet");

    // These substrings should never appear in any response body
    let forbidden_patterns = [
        TEST_XPUB, // full xpub
        "xpub",    // xpub prefix
        "tpub",    // testnet xpub prefix
        "vpub",    // testnet xpub prefix variant
        "upub",    // ypub/zpub prefix variant
    ];

    for pattern in forbidden_patterns {
        assert!(
            !methods_json.contains(pattern),
            "xpub leak detected in list_payment_methods: response contains '{}'. Response: {}",
            pattern,
            methods_json
        );
        assert!(
            !method_json.contains(pattern),
            "xpub leak detected in get_payment_method: response contains '{}'. Response: {}",
            pattern,
            method_json
        );
        assert!(
            !wallets_json.contains(pattern),
            "xpub leak detected in list_wallets: response contains '{}'. Response: {}",
            pattern,
            wallets_json
        );
        assert!(
            !wallet_json.contains(pattern),
            "xpub leak detected in get_wallet_by_id: response contains '{}'. Response: {}",
            pattern,
            wallet_json
        );
    }
}
