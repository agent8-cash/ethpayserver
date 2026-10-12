#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Capability 3 (`invoice_create`), through the production issuer, against a
//! real database.
//!
//! `host_calls.rs`'s own tests drive `PluginHostCalls::invoice_create` end to
//! end, but through a hand-written `FixedIssuer` double that just echoes back
//! whatever it is asked - they prove the JSON dispatch layer forwards a
//! request correctly, and nothing about whether `PluginHostApi`, the type
//! `server.rs` actually publishes at boot, creates a real, payable invoice
//! from it. A bug in `PluginHostApi::invoice_create` itself - the wrong
//! store, a lost decimal converting to the smallest unit, a currency that
//! does not match the payment method it was priced against - would pass
//! every one of those tests, because none of them touch this type.
//!
//! This is the same shape any merchant's customer would pay: a store with a
//! real wallet-backed payment method, invoiced through the exact function a
//! wasm plugin's `invoice_create` import resolves to (see the doc comment on
//! `PluginHostApi`), then read back through the same repositories the rest
//! of the server uses.

use data_service::test_support::pg_service;
use std::sync::Arc;

use async_trait::async_trait;
use sqlx::PgPool;
use uuid::Uuid;

use auth::{Result as AuthResult, Session, SessionId, SessionService, Store, UserId, UserInfo};
use data_service::store_creation::StoreCreationWriter;
use data_service::{InvoiceReader, PaymentOptionReader, PgDataService, StorePaymentMethodWriter};
use payserver_plugin_api::PluginId;
use payserver_plugin_host::PluginHostCalls;
use rates::NoOpRateProvider;
use server::services::RedisEVMMonitor;
use server::services::plugins::{
    DeferredBulkVolume, DeferredCapabilities, DeferredIssuer, DeferredStanding, DeferredVolume,
    HostInvoiceIssuer, PluginCalls, PluginHostApi, PluginPools,
};
use server::state::PgAppState;
use types::ChainId;

/// A real testnet account xpub, reused from `server/src/api/stores/tests.rs`.
/// It has to be real, unlike the placeholder strings the pure repository
/// tests in `data-service` get away with (see that crate's `unique_xpub`):
/// this test creates an invoice for real, which derives a real receiving
/// address from it.
///
/// An xpub belongs to exactly one account (`reject_if_another_account_holds`),
/// so a fresh random owner on every run would collide with whichever earlier
/// run already registered this key - re-registering it to a *different*
/// account is refused, on purpose, as the fix for the bug
/// `20260908120000_account_wallets` describes. [`BILLING_TEST_OWNER`] is
/// fixed for exactly the opposite reason: re-registering the same key to the
/// *same* account is documented as idempotent (`WalletWriter::create_wallet`),
/// so this test can run against a database that already holds its own
/// previous run's data without failing on that account's own key.
const TEST_XPUB: &str = "xpub6DCoCpSuQZB2jawqnGMEPS63ePKWkwWPH4TU45Q7LPXWuNd8TMtVxRrgjtEshuqpK3mdhaWHPFsBngh5GFZaM6si3yZdUsT8ddYM3PwnATt";

/// See [`TEST_XPUB`]. Not a real account; just a stable id this test always
/// reuses so its one real xpub only ever registers to itself.
const BILLING_TEST_OWNER: &str = "00000000-0000-4000-8000-0000000b1111";

struct UnusedSessionService;

#[async_trait]
impl SessionService for UnusedSessionService {
    async fn validate_session(&self, _session_id: SessionId) -> AuthResult<(UserInfo, Session)> {
        unimplemented!("not exercised by invoice_create")
    }
    async fn logout(&self, _session_id: SessionId) -> AuthResult<()> {
        unimplemented!("not exercised by invoice_create")
    }
    async fn logout_all(&self, _session_id: SessionId) -> AuthResult<()> {
        unimplemented!("not exercised by invoice_create")
    }
    async fn cleanup_stale_sessions(&self) -> AuthResult<u64> {
        unimplemented!("not exercised by invoice_create")
    }
}

async fn seed_user(pool: &PgPool) -> Uuid {
    seed_user_with_id(pool, Uuid::new_v4()).await
}

/// `ON CONFLICT DO NOTHING` rather than plain `INSERT`: [`BILLING_TEST_OWNER`]
/// is reused across runs on purpose, and a second run inserting it again must
/// not fail just because the account is already there.
async fn seed_user_with_id(pool: &PgPool, id: Uuid) -> Uuid {
    sqlx::query(
        "INSERT INTO users (id, kdf_params, encrypted_symmetric_key, \
         recovery_verification_hash, kdf_salt_identifier) \
         VALUES ($1, '{}'::jsonb, '{}'::jsonb, 'h', 'passkey:' || $1::text) \
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(id)
    .execute(pool)
    .await
    .expect("seed user");
    id
}

fn app_state(data_service: Arc<PgDataService>) -> PgAppState<UnusedSessionService> {
    PgAppState::new(
        data_service,
        Arc::new(UnusedSessionService),
        None::<Arc<RedisEVMMonitor>>,
        Arc::new(NoOpRateProvider),
        Arc::new(server::services::email::NoopEmailSender),
    )
}

/// Drives a request through the exact JSON boundary a wasm plugin's
/// `invoice_create` import reaches, with the real `PluginHostApi` published
/// behind it - not a test double.
fn issue(issuer: DeferredIssuer, request: &[u8]) -> Result<serde_json::Value, String> {
    let pools = Arc::new(PluginPools::new("postgres://localhost/x".to_string(), 4));
    let calls = PluginCalls::new(PluginId::new("cash.random.billing").unwrap(), pools)
        .with_capabilities(&DeferredCapabilities {
            issuer,
            volume: DeferredVolume::default(),
            bulk_volume: DeferredBulkVolume::default(),
            standing: DeferredStanding::default(),
        });
    let answer = PluginHostCalls::invoice_create(&calls, request)?;
    Ok(serde_json::from_slice(&answer).expect("issuer answered non-JSON"))
}

/// The id is a version-4 UUID: 122 random bits from the CSPRNG. The checkout
/// socket is unauthenticated and filters by invoice id alone, so
/// unguessability is the only thing between a stranger and an invoice's live
/// status. Lowercase is checked on purpose: the server renders ids lowercase,
/// so another case means another generator.
fn assert_v4_invoice_id(id: &str) {
    let parsed = Uuid::parse_str(id).expect("the invoice id must be a UUID");
    assert_eq!(
        parsed.get_version(),
        Some(uuid::Version::Random),
        "invoice id {id} is not a version-4 UUID"
    );
    assert_eq!(
        parsed.get_variant(),
        uuid::Variant::RFC4122,
        "invoice id {id} has the wrong UUID variant"
    );
    assert_eq!(
        parsed.hyphenated().to_string(),
        id,
        "invoice id is not in canonical lowercase hyphenated form"
    );
}

/// `issue` above goes through `PluginHostCalls::invoice_create`, which - like
/// every host call a wasm plugin reaches - runs on a blocking thread and
/// drives its async work with `Handle::current().block_on(..)`
/// (`host_calls.rs`). That panics if it is nested inside an already-running
/// runtime, which a `#[tokio::test]` body is. So these tests are plain
/// `#[test]`s that enter a runtime context (as the crate's own
/// `PluginHostCalls` tests do) and drive setup and readback through
/// `rt.block_on`, calling `issue` synchronously in between - never inside an
/// active `block_on` of its own.
///
/// Ticket's own verify criteria for the underlying capability Subscribe would
/// call: the invoice actually lands in the nominated store, with the plan's
/// price surviving to the base-unit amount a customer would actually pay -
/// not just echoed back by a double.
#[test]
#[ignore]
fn a_real_issuer_creates_a_real_payable_invoice_in_base_units() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();

    let pg = rt.block_on(pg_service());
    let pool = pg.pool().clone();
    let owner = rt.block_on(seed_user_with_id(
        &pool,
        Uuid::parse_str(BILLING_TEST_OWNER).unwrap(),
    ));
    let store = Store::new(format!("store-{}", Uuid::new_v4()), UserId(owner));
    rt.block_on(pg.create_store_owned_by(&store, UserId(owner)))
        .expect("seed store owned by user");

    // A stablecoin-shaped method (6 decimals), the case where a naive
    // decimal shift is most likely to lose or invent a digit.
    rt.block_on(pg.create_payment_method(
        store.id.0,
        &ChainId::evm(11155111),
        None,
        "USDC",
        6,
        Some(TEST_XPUB),
    ))
    .expect("seed a working, wallet-backed payment method");

    let pg = Arc::new(pg);
    let state = app_state(pg.clone());
    let own_store = types::StoreId(store.id.0);
    let api: Arc<dyn HostInvoiceIssuer> = Arc::new(PluginHostApi::new(state, own_store));
    let issuer = DeferredIssuer::new();
    assert!(issuer.publish(api));

    let answer = issue(
        issuer,
        br#"{"asset_symbol":"USDC","amount":"49.99","customer_email":"gus@merchant.example"}"#,
    )
    .expect("a store with a real payment method must be able to issue");

    assert_eq!(answer["currency"], "USDC");
    assert_eq!(
        answer["amount"], "49.99",
        "the invoice must be priced at exactly what was asked, not a lossy conversion"
    );
    assert_eq!(answer["status"], "pending");

    let invoice_id = types::InvoiceId(answer["invoice_id"].as_str().unwrap().to_string());

    assert_v4_invoice_id(&invoice_id.0);

    // Read back through the same repository the HTTP endpoint and the
    // payment pipeline use - proving this is a real row, not a value the
    // issuer merely returned in memory.
    let persisted = rt
        .block_on(InvoiceReader::get(&*pg, &invoice_id))
        .expect("read back the invoice")
        .expect("the invoice was actually persisted");
    assert_eq!(
        persisted.store_id, own_store,
        "the store must come from the issuer's own store, never from the plugin's request"
    );
    // Not a string comparison: the column is NUMERIC(38,18), so Postgres
    // renders it back with trailing zeros ("49.990000000000000000") rather
    // than the digits that were written. The amount must still be exactly
    // 49.99, not a nearby value a lossy float round trip could produce.
    assert_eq!(
        persisted.amount.parse::<rust_decimal::Decimal>().unwrap(),
        "49.99".parse::<rust_decimal::Decimal>().unwrap()
    );
    assert_eq!(
        persisted.customer_email,
        Some("gus@merchant.example".to_string())
    );

    let options = rt
        .block_on(PaymentOptionReader::get_for_invoice(&*pg, &invoice_id))
        .expect("read back payment options");
    assert_eq!(options.len(), 1);
    assert_eq!(
        options[0].amount, "49990000",
        "49.99 USDC at 6 decimals is 49_990_000 base units - a lost or invented \
         digit here is this product charging the wrong amount for itself"
    );
}

/// The 18-decimal case, priced with one more fractional digit than the
/// asset can represent. `USDC`'s 6 decimals in the test above divides
/// `49.99` evenly and can't expose a rounding choice; an 18-decimal,
/// ETH-shaped method priced with 19 fractional digits forces
/// `convert_human_to_smallest_unit` to pick a direction, and this pins it to
/// floor - the same direction `test_convert_floors_result` already pins for
/// the rate-converted path, exercised here end to end against a real
/// database instead of the pure function in isolation.
#[test]
#[ignore]
fn a_real_issuer_floors_precision_the_asset_cannot_represent() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();

    let pg = rt.block_on(pg_service());
    let pool = pg.pool().clone();
    let owner = rt.block_on(seed_user_with_id(
        &pool,
        Uuid::parse_str(BILLING_TEST_OWNER).unwrap(),
    ));
    let store = Store::new(format!("store-{}", Uuid::new_v4()), UserId(owner));
    rt.block_on(pg.create_store_owned_by(&store, UserId(owner)))
        .expect("seed store owned by user");

    rt.block_on(pg.create_payment_method(
        store.id.0,
        &ChainId::evm(11155111),
        None,
        "ETH",
        18,
        Some(TEST_XPUB),
    ))
    .expect("seed a working, wallet-backed payment method");

    let pg = Arc::new(pg);
    let state = app_state(pg.clone());
    let own_store = types::StoreId(store.id.0);
    let api: Arc<dyn HostInvoiceIssuer> = Arc::new(PluginHostApi::new(state, own_store));
    let issuer = DeferredIssuer::new();
    assert!(issuer.publish(api));

    // 0.1234567890123456789 has 19 fractional digits against an 18-decimal
    // asset: `* 10^18` leaves a trailing 0.9 of a base unit, which must be
    // floored away, not rounded up (that would invent a unit the merchant
    // never priced) or truncated on the wrong digit (either over- or
    // undercharging the customer for the same subscription).
    let answer = issue(
        issuer,
        br#"{"asset_symbol":"ETH","amount":"0.1234567890123456789"}"#,
    )
    .expect("a store with a real payment method must be able to issue");

    let invoice_id = types::InvoiceId(answer["invoice_id"].as_str().unwrap().to_string());
    let options = rt
        .block_on(PaymentOptionReader::get_for_invoice(&*pg, &invoice_id))
        .expect("read back payment options");
    assert_eq!(options.len(), 1);
    assert_eq!(
        options[0].amount, "123456789012345678",
        "19 fractional digits against an 18-decimal asset must floor to the \
         base unit, never round up and invent money or truncate the wrong digit"
    );
}

/// The negative case for the same real issuer: a store with no payment
/// method for the requested asset must refuse, not silently invoice in
/// something the merchant never configured.
#[test]
#[ignore]
fn a_real_issuer_refuses_an_asset_the_store_has_not_configured() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();

    let pg = rt.block_on(pg_service());
    let owner = rt.block_on(seed_user(pg.pool()));
    let store = Store::new(format!("store-{}", Uuid::new_v4()), UserId(owner));
    rt.block_on(pg.create_store_owned_by(&store, UserId(owner)))
        .expect("seed store owned by user");

    let pg = Arc::new(pg);
    let state = app_state(pg.clone());
    let own_store = types::StoreId(store.id.0);
    let api: Arc<dyn HostInvoiceIssuer> = Arc::new(PluginHostApi::new(state, own_store));
    let issuer = DeferredIssuer::new();
    assert!(issuer.publish(api));

    let err = issue(issuer, br#"{"asset_symbol":"USDC","amount":"49.99"}"#)
        .expect_err("a store with no matching payment method must refuse");
    assert!(err.contains("no enabled payment method"), "{err}");
}
