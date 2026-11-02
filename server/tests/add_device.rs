#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `POST /users/me/devices`, driven through the production router with a
//! real session against a real Postgres.
//!
//! SENSITIVE: covers device enrollment and the device cap. See
//! `server/src/api/users/devices.rs`.
//!
//! Needs `DATABASE_URL` and fails, naming it, when it is unset; runs in CI's
//! `--run-ignored` step.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{Duration, Utc};
use data_service::PgDataService;
use data_service::test_support::pg_service;
use rates::NoOpRateProvider;
use serde_json::{Value, json};
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

/// A seeded user with throwaway-but-shape-valid crypto columns, matching the
/// pattern `cross_tenant_isolation::support::seed_user` and `user_signup`'s
/// `seed_user` both use - the handler under test never decodes these, so
/// their actual bytes do not matter, only that they parse as the real
/// `crypto::KdfParams`/`EncryptedBlob` shapes.
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

/// A device + session row for `user_id`, with the session's `created_at`
/// set explicitly - the one thing `FreshlyAuthenticatedUser` reads.
async fn seed_session(pool: &PgPool, user_id: Uuid, created_at: chrono::DateTime<Utc>) -> Uuid {
    let device_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO devices (id, user_id, name, device_type, encrypted_symmetric_key, kdf_params, is_active) \
         VALUES ($1, $2, 'seed device', 'browser', \
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
         VALUES ($1, $2, $3, $4, NOW() + INTERVAL '1 day', NOW())",
    )
    .bind(session_id)
    .bind(user_id)
    .bind(device_id)
    .bind(created_at)
    .execute(pool)
    .await
    .expect("seed session");
    session_id
}

/// An extra, already-active device for `user_id` - used to fill the cap
/// without going through the handler under test.
async fn seed_extra_active_device(pool: &PgPool, user_id: Uuid) {
    sqlx::query(
        "INSERT INTO devices (id, user_id, name, device_type, encrypted_symmetric_key, kdf_params, is_active) \
         VALUES ($1, $2, 'filler device', 'browser', \
             '{\"ciphertext\":\"AAAA\",\"iv\":\"AAAA\",\"mac\":\"AAAA\"}'::jsonb, \
             '{\"algorithm\":\"argon2id\",\"memory_kb\":65536,\"iterations\":3,\"parallelism\":4,\"salt\":\"AAAA\"}'::jsonb, \
             TRUE)",
    )
    .bind(Uuid::new_v4())
    .bind(user_id)
    .execute(pool)
    .await
    .expect("seed extra device");
}

fn add_device_body(name: &str) -> Value {
    json!({
        "device_name": name,
        "device_type": "browser",
        // Base64 of arbitrary bytes - the server never decrypts or
        // otherwise interprets this, only base64-decodes the envelope
        // fields (see `crypto::EncryptedBlob`'s `Deserialize`).
        "encrypted_symmetric_key": {
            "ciphertext": "bm90LXRoZS1yZWFsLWtleQ==",
            "iv": "aXZpdml2aXZpdml2aXZpdg==",
            "mac": "bWFjbWFjbWFjbWFjbWFjbWFjbWFjbWFjbWFjbWFjbWFjbWFjbWFjbWFjbWFjPQ=="
        },
        "kdf_params": {
            "algorithm": "argon2id",
            "memory_kb": 65536,
            "iterations": 3,
            "parallelism": 4,
            "salt": "c29tZS1zYWx0"
        }
    })
}

async fn post_devices(app: &Router, token: Option<&str>, body: Value) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method("POST")
        .uri("/users/me/devices")
        .header("content-type", "application/json");
    if let Some(token) = token {
        req = req.header("authorization", format!("Bearer {token}"));
    }
    let resp = app
        .clone()
        .oneshot(req.body(Body::from(body.to_string())).unwrap())
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

#[tokio::test]
#[ignore]
async fn without_a_session_the_request_is_refused() {
    let pg = Arc::new(pg_service().await);
    let (status, _) = post_devices(&app(&pg), None, add_device_body("no session")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
#[ignore]
async fn a_stale_session_is_refused_even_though_it_is_still_valid() {
    let pg = Arc::new(pg_service().await);
    let user_id = seed_user(pg.pool()).await;
    // Created well outside the 5-minute freshness window `FreshlyAuthenticatedUser`
    // enforces (`server/src/api/auth_freshness.rs`), but inside the session's
    // own 1-day expiry - this is "logged in a while ago", not "logged out".
    let session_id = seed_session(pg.pool(), user_id, Utc::now() - Duration::minutes(30)).await;

    let (status, _) = post_devices(
        &app(&pg),
        Some(&session_id.to_string()),
        add_device_body("stale"),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
#[ignore]
async fn a_fresh_session_enrolls_a_device_and_the_reply_never_carries_key_material() {
    let pg = Arc::new(pg_service().await);
    let user_id = seed_user(pg.pool()).await;
    let session_id = seed_session(pg.pool(), user_id, Utc::now()).await;

    let (status, body) = post_devices(
        &app(&pg),
        Some(&session_id.to_string()),
        add_device_body("new laptop"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "new laptop");
    assert_eq!(body["device_type"], "browser");
    assert_eq!(body["is_active"], true);
    // `DeviceInfo` is the sanitized shape on purpose (see its doc comment in
    // the `auth` crate) - this pins that the client-supplied wrapped key
    // never comes back on the wire.
    assert!(body.get("encrypted_symmetric_key").is_none());
    assert!(body.get("kdf_params").is_none());

    let stored: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM devices WHERE user_id = $1 AND name = 'new laptop' AND is_active",
    )
    .bind(user_id)
    .fetch_one(pg.pool())
    .await
    .unwrap();
    assert_eq!(stored, 1, "the device row must actually be persisted");
}

#[tokio::test]
#[ignore]
async fn the_device_cap_is_enforced_before_the_row_is_created() {
    let pg = Arc::new(pg_service().await);
    let user_id = seed_user(pg.pool()).await;
    let session_id = seed_session(pg.pool(), user_id, Utc::now()).await;

    // One active device already exists from `seed_session`. Fill the rest of
    // the cap directly, bypassing the handler under test.
    let cap = AuthConfig::default().max_devices_per_user;
    for _ in 1..cap {
        seed_extra_active_device(pg.pool(), user_id).await;
    }

    let before: i64 =
        sqlx::query_scalar("SELECT count(*) FROM devices WHERE user_id = $1 AND is_active")
            .bind(user_id)
            .fetch_one(pg.pool())
            .await
            .unwrap();
    assert_eq!(
        before as u32, cap,
        "test setup must land exactly at the cap"
    );

    let (status, _) = post_devices(
        &app(&pg),
        Some(&session_id.to_string()),
        add_device_body("one too many"),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    let after: i64 =
        sqlx::query_scalar("SELECT count(*) FROM devices WHERE user_id = $1 AND is_active")
            .bind(user_id)
            .fetch_one(pg.pool())
            .await
            .unwrap();
    assert_eq!(after, before, "a refused enrollment must not create a row");
}

#[tokio::test]
#[ignore]
async fn a_malformed_body_is_rejected_rather_than_silently_coerced() {
    let pg = Arc::new(pg_service().await);
    let user_id = seed_user(pg.pool()).await;
    let session_id = seed_session(pg.pool(), user_id, Utc::now()).await;

    // Unknown field: `#[serde(deny_unknown_fields)]` on `AddDeviceRequest`
    // means this is refused outright rather than silently ignoring a field a
    // client-side typo sent, mirroring `SignupRequest`'s own convention.
    let mut body = add_device_body("x");
    body["unexpected"] = json!("field");

    let (status, _) = post_devices(&app(&pg), Some(&session_id.to_string()), body).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}
