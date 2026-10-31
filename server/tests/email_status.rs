#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `GET /users/me/email/status`, driven through the production router with a
//! real `Authorization: Bearer` key against a real Postgres.
//!
//! Pins the response shape a client reads: `configured` and the email-change
//! verification window in minutes, so the client does not hardcode it.
//!
//! Needs `DATABASE_URL` and fails, naming it, when it is unset; runs in CI's
//! `--run-ignored` step.

use data_service::test_support::pg_service;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use data_service::PgDataService;
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

/// A merchant user with an unscoped key. Returns the raw key.
async fn seed_user(pg: &PgDataService) -> String {
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

    let key = format!("ak_email_status_{}", Uuid::new_v4());
    sqlx::query(
        "INSERT INTO api_keys (id, user_id, name, key_hash, key_prefix, permissions) \
         VALUES ($1, $2, 'email status key', $3, 'ak_test', NULL)",
    )
    .bind(Uuid::new_v4())
    .bind(user_id)
    .bind(hex::encode(Sha256::digest(key.as_bytes())))
    .execute(pg.pool())
    .await
    .expect("seed api key");
    key
}

#[tokio::test]
#[ignore]
async fn email_status_reports_the_email_change_window_in_minutes() {
    let pg = Arc::new(pg_service().await);
    let key = seed_user(&pg).await;

    let req = Request::builder()
        .method("GET")
        .uri("/users/me/email/status")
        .header("authorization", format!("Bearer {key}"))
        .body(Body::empty())
        .unwrap();
    let resp = app(&pg).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();

    assert_eq!(body["configured"], false);
    assert_eq!(body["change_ttl_minutes"], 30);
}
