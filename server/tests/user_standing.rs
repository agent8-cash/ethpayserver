#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `GET /users/me/standing`, driven through the production router with a real
//! `Authorization: Bearer` key against a real Postgres.
//!
//! Pins the properties a pure-function test cannot: the route sits behind
//! authentication, the lookup is keyed on the caller's own user id, and one
//! merchant never sees another's standing.
//!
//! Needs `DATABASE_URL` and fails, naming it, when it is unset; runs in CI's
//! `--run-ignored` step.

use data_service::test_support::pg_service;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use chrono::{Duration, Utc};
use data_service::{AccountStanding, AccountStandingStore, PgDataService};
use rates::NoOpRateProvider;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tower::ServiceExt;
use uuid::Uuid;

use auth::{AuthConfig, AuthService};
use server::services::RedisEVMMonitor;
use server::state::PgAppState;

fn app(pg: &Arc<PgDataService>) -> Router {
    let auth_service = Arc::new(AuthService::with_config(
        Arc::clone(pg),
        AuthConfig::default(),
    ));
    let state = PgAppState::new(
        Arc::clone(pg),
        auth_service,
        None::<Arc<RedisEVMMonitor>>,
        Arc::new(NoOpRateProvider),
        Arc::new(server::services::email::NoopEmailSender),
    );
    server::api::router(state, false, None, None, None)
}

/// A merchant user with an unscoped key. Returns the user id and the raw key.
async fn seed_user(pg: &PgDataService) -> (Uuid, String) {
    let user_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, kdf_params, encrypted_symmetric_key, \
         recovery_verification_hash, kdf_salt_identifier) \
         VALUES ($1, \
             '{\"algorithm\":\"argon2id\",\"memory_kb\":65536,\"iterations\":3,\"parallelism\":4,\"salt\":\"\"}'::jsonb, \
             '{\"ciphertext\":\"\",\"iv\":\"\",\"mac\":\"\"}'::jsonb, \
             'h', 'passkey:' || $1::text)",
    )
    .bind(user_id)
    .execute(pg.pool())
    .await
    .expect("seed user");

    let key = format!("ak_standing_read_{}", Uuid::new_v4());
    sqlx::query(
        "INSERT INTO api_keys (id, user_id, name, key_hash, key_prefix, permissions) \
         VALUES ($1, $2, 'standing read key', $3, 'ak_test', NULL)",
    )
    .bind(Uuid::new_v4())
    .bind(user_id)
    .bind(hex::encode(Sha256::digest(key.as_bytes())))
    .execute(pg.pool())
    .await
    .expect("seed api key");
    (user_id, key)
}

/// The response headers as well as the body. The header set is part of what this
/// route exposes, and a test that throws it away cannot see a field arrive there.
async fn get_standing_full(app: &Router, key: Option<&str>) -> (StatusCode, HeaderMap, Value) {
    let mut req = Request::builder().method("GET").uri("/users/me/standing");
    if let Some(key) = key {
        req = req.header("authorization", format!("Bearer {key}"));
    }
    let resp = app
        .clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        headers,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn get_standing(app: &Router, key: Option<&str>) -> (StatusCode, Value) {
    let (status, _, body) = get_standing_full(app, key).await;
    (status, body)
}

async fn hold(pg: &PgDataService, account: Uuid, good: bool, plan: &str, days: i64) {
    pg.apply_account_standing(&AccountStanding {
        account_id: account,
        version: 1,
        in_good_standing: good,
        paid_through: Some(Utc::now() + Duration::days(days)),
        plan_name: plan.to_string(),
        checkout_url: Some("https://pay.example/checkout".to_string()),
    })
    .await
    .unwrap();
}

#[tokio::test]
#[ignore]
async fn an_unauthenticated_read_is_refused() {
    let pg = Arc::new(pg_service().await);
    let (status, _) = get_standing(&app(&pg), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
#[ignore]
async fn a_merchant_reads_their_own_standing_and_never_anothers() {
    let pg = Arc::new(pg_service().await);
    let app = app(&pg);
    let (lapsed_id, lapsed_key) = seed_user(&pg).await;
    let (good_id, good_key) = seed_user(&pg).await;
    let (_, unheard_key) = seed_user(&pg).await;
    hold(&pg, lapsed_id, false, "plan-lapsed", 30).await;
    hold(&pg, good_id, true, "plan-good", 30).await;

    let (status, body) = get_standing(&app, Some(&lapsed_key)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["state"], "lapsed");
    assert_eq!(body["plan_name"], "plan-lapsed");
    assert_eq!(body["checkout_url"], "https://pay.example/checkout");

    let (status, body) = get_standing(&app, Some(&good_key)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["state"], "good");
    assert_eq!(body["plan_name"], "plan-good");

    let (status, body) = get_standing(&app, Some(&unheard_key)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["state"], "unknown");
    assert_eq!(body["plan_name"], Value::Null);
}

/// An authenticated read of the caller's own standing, seeded INSIDE the
/// seven-day warning window. The window matters: the sibling tests seed thirty
/// days out, where no expiry warning is ever due, and a response-header
/// assertion made there would stay green through exactly the change it exists to
/// catch.
///
/// The header assertion below is only meaningful because `app()` builds the
/// PRODUCTION router - `server::api::router(..)` - and every response-header
/// middleware in this service is applied inside it as a whole-router
/// `app.layer(..)`, not per-route. So a layer added to production is in this test
/// too. A bare router assembled here would make that test unable to fail.
async fn standing_of_an_expiring_account() -> (HeaderMap, Value) {
    let pg = Arc::new(pg_service().await);
    let (user, key) = seed_user(&pg).await;
    hold(&pg, user, true, "plan-expiring", 2).await;
    let (status, headers, body) = get_standing_full(&app(&pg), Some(&key)).await;
    assert_eq!(status, StatusCode::OK);
    (headers, body)
}

/// ALLOW-LIST ON THE BODY. The stored standing holds more than this route is
/// meant to hand back, and nothing asserted the limit, so a field added to the
/// response type that mirrors it reached the wire by default.
///
/// These four names are the whole of what a merchant reading their own standing
/// receives, and each is here on purpose: the state, the plan they are on, when
/// it runs out, where to pay. Adding a fifth is fine - amend this list in the
/// same commit, which is the point.
///
/// Separate test from the header allow-list, deliberately. The two were one test
/// and the ablation proved only the first assertion: a panic on the body list
/// meant the header list was never reached, so it was an unproven control
/// sitting behind a proven one.
#[tokio::test]
#[ignore]
async fn the_standing_read_exposes_only_these_four_fields() {
    let (_, body) = standing_of_an_expiring_account().await;

    let mut fields: Vec<&str> = body
        .as_object()
        .expect("the standing read returns a json object")
        .keys()
        .map(String::as_str)
        .collect();
    fields.sort_unstable();
    assert_eq!(
        fields,
        ["checkout_url", "paid_through", "plan_name", "state"],
        "this route hands back exactly these four facts; a new one must be added \
         to this list in the commit that adds it, so that exposing it is a decision \
         someone made rather than a default"
    );
}

/// Every header name this response may carry. Anything else is a new exposure
/// and fails the test below until someone adds it here on purpose.
///
/// The list is the transport's own, because that is all this route sends today.
const ALLOWED_RESPONSE_HEADERS: &[&str] = &["content-length", "content-type"];

/// ALLOW-LIST ON THE HEADERS. The whole set, not a prefix.
///
/// It was `starts_with("x-account")` first, and that was a test that could not
/// fail for the case it was written to catch. The ticket names `Warning` as a
/// candidate header as readily as an `X-` name, so an implementation stamping
/// `Warning`, `x-subscription-*` or `x-expiry` would have left it green while its
/// own doc comment promised the opposite. Worse, the ablation that "proved" it
/// used `x-account-ablation` - a name chosen to match the filter - so it only
/// ever demonstrated that the filter matches what the filter matches.
///
/// Asserting the complete set removes the implementer's naming choice from the
/// question: any header this route does not already send fails by default, which
/// is the property the body allow-list above already has.
///
/// A header is the broader of the two exposures. It rides on responses the caller
/// never asked for, and intermediaries log headers that they do not log bodies -
/// so the surface that grows without anyone naming it is the one worth pinning
/// before it exists.
#[tokio::test]
#[ignore]
async fn the_standing_read_carries_only_transport_headers() {
    let (headers, _) = standing_of_an_expiring_account().await;

    let unexpected: Vec<String> = headers
        .keys()
        .map(|name| name.as_str().to_ascii_lowercase())
        .filter(|name| !ALLOWED_RESPONSE_HEADERS.contains(&name.as_str()))
        .collect();
    assert!(
        unexpected.is_empty(),
        "this response carries a header that is not in ALLOWED_RESPONSE_HEADERS: \
         {unexpected:?}. A header reaches callers who did not ask for it, and \
         intermediaries log headers they do not log bodies, so adding one is a \
         decision that belongs in the same commit as the list entry - whatever it \
         is called. If the expiry warning landed, name its headers here."
    );
}
