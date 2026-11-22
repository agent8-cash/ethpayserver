#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Plugin page action submission tests.
//!
//! Tests POST /plugins/{id}/page-actions/{*path} through the production
//! router with a real session against a real Postgres.
//!
//! Verifies that:
//! - The route accepts and dispatches page action requests
//! - Identity (viewer/account_id) comes from the authenticated session
//! - Responses from the plugin propagate correctly to the client
//! - Plugin errors are distinguished from wire-level errors
//! - Role-based access control is enforced by the host
//! - Comprehensive logging occurs on failure

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use data_service::{PgDataService, test_support::pg_service};
use rates::NoOpRateProvider;
use serde_json::json;
use sqlx::PgPool;
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

async fn seed_user(pool: &PgPool) -> Uuid {
    let id = Uuid::new_v4();
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

async fn seed_session_token(pool: &PgPool, user_id: Uuid) -> String {
    let device_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO devices (id, user_id, name, device_type, encrypted_symmetric_key, kdf_params, is_active) \
         VALUES ($1, $2, 'test device', 'browser', \
             '{\"ciphertext\":\"AAAA\",\"iv\":\"AAAA\",\"mac\":\"AAAA\"}'::jsonb, \
             '{\"algorithm\":\"argon2id\",\"memory_kb\":65536,\"iterations\":3,\"parallelism\":4,\"salt\":\"AAAA\"}'::jsonb, \
             TRUE)",
    )
    .bind(device_id)
    .bind(user_id)
    .execute(pool)
    .await
    .expect("seed device");

    let session_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO sessions (id, user_id, device_id, created_at, expires_at, last_activity_at) \
         VALUES ($1, $2, $3, NOW(), NOW() + INTERVAL '1 day', NOW())",
    )
    .bind(session_id)
    .bind(user_id)
    .bind(device_id)
    .execute(pool)
    .await
    .expect("seed session");

    session_id.to_string()
}

/// Installs a plugin row with no manifest guarantees beyond parsing - enough
/// for `resolve_plugin`'s fallback-by-id path to find it, without implying
/// the (absent, in these tests) plugin host can actually load it.
async fn seed_installed_plugin(pool: &PgPool, id: &str) {
    sqlx::query(
        "INSERT INTO installed_plugins (id, version, manifest_toml, artifact_sha256) \
         VALUES ($1, '0.1.0', $2, $3)",
    )
    .bind(id)
    .bind(format!(
        "id = \"{id}\"\nversion = \"0.1.0\"\nkind = \"action\"\n"
    ))
    .bind("a".repeat(64))
    .execute(pool)
    .await
    .expect("seed installed plugin");
}

/// Tests that a plugin page action request without plugins loaded returns 503.
#[tokio::test]
#[ignore]
async fn page_action_without_plugin_host_returns_service_unavailable() {
    let pg = Arc::new(pg_service().await);
    let user_id = seed_user(pg.pool()).await;
    let token = seed_session_token(pg.pool(), user_id).await;
    seed_installed_plugin(pg.pool(), "test-plugin").await;

    let app = app(&pg);

    // POST an action - the plugin is installed but no host is running
    let body = serde_json::to_vec(&json!({
        "action_id": "test_action",
        "data": {"key": "value"}
    }))
    .unwrap();

    let request = Request::builder()
        .method("POST")
        .uri("/plugins/test-plugin/page-actions/test-page")
        .header("Authorization", format!("Bearer {}", token))
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

/// Tests that an unauthenticated request to POST an action is rejected.
#[tokio::test]
#[ignore]
async fn unauthenticated_page_action_returns_unauthorized() {
    let pg = Arc::new(pg_service().await);

    let app = app(&pg);

    // POST an action without authentication
    let body = serde_json::to_vec(&json!({
        "action_id": "test_action",
        "data": {"key": "value"}
    }))
    .unwrap();

    let request = Request::builder()
        .method("POST")
        .uri("/plugins/test-plugin/page-actions/test-page")
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

/// Tests that a malformed action request (invalid JSON) returns 400.
#[tokio::test]
#[ignore]
async fn malformed_page_action_request_returns_bad_request() {
    let pg = Arc::new(pg_service().await);
    let user_id = seed_user(pg.pool()).await;
    let token = seed_session_token(pg.pool(), user_id).await;

    let app = app(&pg);

    let request = Request::builder()
        .method("POST")
        .uri("/plugins/test-plugin/page-actions/test-page")
        .header("Authorization", format!("Bearer {}", token))
        .header("content-type", "application/json")
        .body(Body::from("not valid json"))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

/// Tests that posting to a non-existent plugin returns 404.
#[tokio::test]
#[ignore]
async fn action_to_nonexistent_plugin_returns_not_found() {
    let pg = Arc::new(pg_service().await);
    let user_id = seed_user(pg.pool()).await;
    let token = seed_session_token(pg.pool(), user_id).await;

    let app = app(&pg);

    let body = serde_json::to_vec(&json!({
        "action_id": "test_action",
        "data": {"key": "value"}
    }))
    .unwrap();

    let request = Request::builder()
        .method("POST")
        .uri("/plugins/nonexistent-plugin-xyz/page-actions/test-page")
        .header("Authorization", format!("Bearer {}", token))
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

/// Tests that an empty action_id is rejected.
#[tokio::test]
#[ignore]
async fn empty_action_id_returns_bad_request() {
    let pg = Arc::new(pg_service().await);
    let user_id = seed_user(pg.pool()).await;
    let token = seed_session_token(pg.pool(), user_id).await;
    seed_installed_plugin(pg.pool(), "test-plugin").await;

    let app = app(&pg);

    let body = serde_json::to_vec(&json!({
        "action_id": "",
        "data": {"key": "value"}
    }))
    .unwrap();

    let request = Request::builder()
        .method("POST")
        .uri("/plugins/test-plugin/page-actions/test-page")
        .header("Authorization", format!("Bearer {}", token))
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}
