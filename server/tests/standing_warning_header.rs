#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The `X-Account-Warning` header reaches a real response.
//!
//! The middleware's own tests fill the account slot by hand, so they cannot
//! notice the authenticating extractor stopping to fill it, or the layer not
//! being mounted. These drive the router `main` serves with a real API key and
//! a real session, and hold a real standing in Postgres.
//!
//! Needs `DATABASE_URL`, like the other ignored integration tests.

use data_service::test_support::pg_service;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, Response, StatusCode};
use chrono::{Duration, Utc};
use sha2::{Digest, Sha256};
use tower::ServiceExt;
use uuid::Uuid;

use auth::{AuthConfig, AuthService};
use data_service::{AccountStanding, AccountStandingStore, PgDataService};
use rates::NoOpRateProvider;
use server::api::standing_warning::X_ACCOUNT_WARNING;
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

/// A user with an API key and a session; returns `(user, key, session)`.
async fn seed(pg: &PgDataService) -> (Uuid, String, Uuid) {
    let user = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, kdf_params, encrypted_symmetric_key, \
         recovery_verification_hash, kdf_salt_identifier) \
         VALUES ($1, \
             '{\"algorithm\":\"argon2id\",\"memory_kb\":65536,\"iterations\":3,\"parallelism\":4,\"salt\":\"\"}'::jsonb, \
             '{\"ciphertext\":\"\",\"iv\":\"\",\"mac\":\"\"}'::jsonb, \
             'h', 'passkey:' || $1::text)",
    )
    .bind(user)
    .execute(pg.pool())
    .await
    .expect("seed user");

    let key = format!("ak_test_{}", Uuid::new_v4());
    sqlx::query(
        "INSERT INTO api_keys (id, user_id, name, key_hash, key_prefix) \
         VALUES ($1, $2, 'warning test key', $3, 'ak_test')",
    )
    .bind(Uuid::new_v4())
    .bind(user)
    .bind(hex::encode(Sha256::digest(key.as_bytes())))
    .execute(pg.pool())
    .await
    .expect("seed api key");

    let device = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO devices (id, user_id, name, encrypted_symmetric_key, kdf_params) \
         VALUES ($1, $2, 'warning test device', '{\"ciphertext\":\"\",\"iv\":\"\",\"mac\":\"\"}'::jsonb, \
             '{\"algorithm\":\"argon2id\",\"memory_kb\":65536,\"iterations\":3,\"parallelism\":4,\"salt\":\"\"}'::jsonb)",
    )
    .bind(device)
    .bind(user)
    .execute(pg.pool())
    .await
    .expect("seed device");
    let session = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO sessions (id, user_id, device_id, expires_at) \
         VALUES ($1, $2, $3, NOW() + interval '1 hour')",
    )
    .bind(session)
    .bind(user)
    .bind(device)
    .execute(pg.pool())
    .await
    .expect("seed session");
    (user, key, session)
}

async fn hold(pg: &PgDataService, account: Uuid, days_left: i64) {
    pg.apply_account_standing(&AccountStanding {
        account_id: account,
        version: 1,
        in_good_standing: true,
        paid_through: Some(Utc::now() + Duration::days(days_left)),
        plan_name: "p".into(),
        checkout_url: Some("https://pay.example/c".into()),
    })
    .await
    .expect("hold standing");
}

async fn get(app: &Router, token: &str) -> Response<Body> {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/stores")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("router call");
    let status = response.status();
    assert_eq!(status, StatusCode::OK, "GET /stores");
    response
}

#[tokio::test]
#[ignore]
async fn key_and_session_callers_inside_the_window_get_the_header() {
    let pg = Arc::new(pg_service().await);
    let (user, key, session) = seed(&pg).await;
    hold(&pg, user, 2).await;
    let app = app(&pg);
    for token in [key, session.to_string()] {
        let response = get(&app, &token).await;
        let text = response
            .headers()
            .get(&X_ACCOUNT_WARNING)
            .expect("header present inside the window")
            .to_str()
            .unwrap();
        assert!(text.contains("renew at https://pay.example/c"), "{text}");
    }
}

#[tokio::test]
#[ignore]
async fn a_caller_outside_the_window_gets_no_header() {
    let pg = Arc::new(pg_service().await);
    let (user, key, _) = seed(&pg).await;
    hold(&pg, user, 30).await;
    let response = get(&app(&pg), &key).await;
    assert!(response.headers().get(&X_ACCOUNT_WARNING).is_none());
}
