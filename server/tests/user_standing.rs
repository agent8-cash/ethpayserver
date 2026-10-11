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
use axum::http::{Request, StatusCode};
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

async fn get_standing(app: &Router, key: Option<&str>) -> (StatusCode, Value) {
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
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn hold(pg: &PgDataService, account: Uuid, good: bool, plan: &str) {
    pg.apply_account_standing(&AccountStanding {
        account_id: account,
        version: 1,
        in_good_standing: good,
        paid_through: Some(Utc::now() + Duration::days(30)),
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
    hold(&pg, lapsed_id, false, "plan-lapsed").await;
    hold(&pg, good_id, true, "plan-good").await;

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
