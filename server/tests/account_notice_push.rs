#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The account-notice route, driven through the production router with a
//! real `Authorization: Bearer` key against a real Postgres.
//!
//! Mirrors `account_standing_push.rs`: every credential is exercised as
//! itself, and a key without the notice scope is refused as that key, not as
//! an admin.
//!
//! Needs `DATABASE_URL` and fails, naming it, when it is unset or the
//! database is unreachable, like the other ignored integration tests; runs
//! in CI's `--run-ignored` step.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use data_service::{PgDataService, test_support::pg_service};
use rates::NoOpRateProvider;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tower::ServiceExt;
use uuid::Uuid;

use auth::{AuthConfig, AuthService};
use server::services::RedisEVMMonitor;
use server::services::email::{
    AccountNotice, EmailChangeVerificationData, EmailError, EmailSender, ReceiptData,
};
use server::state::PgAppState;

const NOTICE_SCOPE: &str = "ethpay.server.cansendaccountnotice";

/// Fails every send, so a test can drive the route's 502 path without a real
/// SMTP transport.
struct FailingSender;

#[async_trait]
impl EmailSender for FailingSender {
    async fn send_receipt(&self, _to: &str, _data: &ReceiptData) -> Result<(), EmailError> {
        unimplemented!("not exercised by this route")
    }
    async fn send_email_change_verification(
        &self,
        _to: &str,
        _data: &EmailChangeVerificationData,
    ) -> Result<(), EmailError> {
        unimplemented!("not exercised by this route")
    }
    async fn send_account_notice(
        &self,
        _to: &str,
        _notice: &AccountNotice,
    ) -> Result<(), EmailError> {
        Err(EmailError::NotConfigured)
    }
    fn is_configured(&self) -> bool {
        true
    }
}

/// Records every delivered notice, so a test can assert the exact subject
/// and body reached the sender for the address it resolved.
#[derive(Default)]
struct RecordingSender {
    calls: Mutex<Vec<(String, AccountNotice)>>,
}

#[async_trait]
impl EmailSender for RecordingSender {
    async fn send_receipt(&self, _to: &str, _data: &ReceiptData) -> Result<(), EmailError> {
        unimplemented!("not exercised by this route")
    }
    async fn send_email_change_verification(
        &self,
        _to: &str,
        _data: &EmailChangeVerificationData,
    ) -> Result<(), EmailError> {
        unimplemented!("not exercised by this route")
    }
    async fn send_account_notice(
        &self,
        to: &str,
        notice: &AccountNotice,
    ) -> Result<(), EmailError> {
        self.calls
            .lock()
            .unwrap()
            .push((to.to_string(), notice.clone()));
        Ok(())
    }
    fn is_configured(&self) -> bool {
        true
    }
}

async fn service() -> Arc<PgDataService> {
    Arc::new(pg_service().await)
}

fn app(pg: &Arc<PgDataService>, email_sender: Arc<dyn EmailSender>) -> Router {
    let auth_service = Arc::new(AuthService::with_config(
        Arc::clone(pg),
        AuthConfig::default(),
    ));
    let state = PgAppState::new(
        Arc::clone(pg),
        auth_service,
        None::<Arc<RedisEVMMonitor>>,
        Arc::new(NoOpRateProvider),
        email_sender,
    );
    server::api::router(state, false, None, None, None)
}

/// A user with the given role, email and a key with the given stored scope
/// (`None` = an unscoped key). Returns `(account_id, raw key)`.
async fn seed_key(
    pg: &PgDataService,
    role: &str,
    email: Option<&str>,
    scope: Option<&[&str]>,
) -> (Uuid, String) {
    let user_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, email, kdf_params, encrypted_symmetric_key, \
         recovery_verification_hash, kdf_salt_identifier, role) \
         VALUES ($1, $2, \
             '{\"algorithm\":\"argon2id\",\"memory_kb\":65536,\"iterations\":3,\"parallelism\":4,\"salt\":\"\"}'::jsonb, \
             '{\"ciphertext\":\"\",\"iv\":\"\",\"mac\":\"\"}'::jsonb, \
             'h', 'passkey:' || $1::text, $3)",
    )
    .bind(user_id)
    .bind(email)
    .bind(role)
    .execute(pg.pool())
    .await
    .expect("seed user");

    let key = format!("ak_notice_{}", Uuid::new_v4());
    let scope: Option<Vec<String>> = scope.map(|s| s.iter().map(|e| e.to_string()).collect());
    sqlx::query(
        "INSERT INTO api_keys (id, user_id, name, key_hash, key_prefix, permissions) \
         VALUES ($1, $2, 'notice test key', $3, 'ak_test', $4)",
    )
    .bind(Uuid::new_v4())
    .bind(user_id)
    .bind(hex::encode(Sha256::digest(key.as_bytes())))
    .bind(scope)
    .execute(pg.pool())
    .await
    .expect("seed api key");
    (user_id, key)
}

async fn send(
    app: &Router,
    method: &str,
    uri: &str,
    key: Option<&str>,
    body: Vec<u8>,
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json");
    if let Some(key) = key {
        req = req.header("authorization", format!("Bearer {key}"));
    }
    let resp = app
        .clone()
        .oneshot(req.body(Body::from(body)).unwrap())
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

fn notice_body(account: Uuid, subject: &str, body: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({"account_id": account, "subject": subject, "body": body})).unwrap()
}

async fn notify(app: &Router, key: Option<&str>, body: Vec<u8>) -> (StatusCode, Value) {
    send(app, "POST", "/plugins/account-notices", key, body).await
}

#[tokio::test]
#[ignore]
async fn an_account_with_an_email_is_delivered_and_the_sender_sees_the_resolved_address() {
    let pg = service().await;
    let recorder = Arc::new(RecordingSender::default());
    let app = app(&pg, recorder.clone());
    let (_, key) = seed_key(&pg, "server_admin", None, Some(&[NOTICE_SCOPE])).await;
    let email = format!("merchant-{}@example.com", Uuid::new_v4());
    let (account, _) = seed_key(&pg, "user", Some(&email), None).await;

    let (status, body) = notify(
        &app,
        Some(&key),
        notice_body(account, "subject", "body text"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"delivered": true}));

    let calls = recorder.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, email);
    assert_eq!(calls[0].1.subject, "subject");
    assert_eq!(calls[0].1.body, "body text");
}

#[tokio::test]
#[ignore]
async fn a_wallet_only_account_is_silent_not_failing() {
    let pg = service().await;
    let recorder = Arc::new(RecordingSender::default());
    let app = app(&pg, recorder.clone());
    let (_, key) = seed_key(&pg, "server_admin", None, Some(&[NOTICE_SCOPE])).await;
    let (account, _) = seed_key(&pg, "user", None, None).await;

    let (status, body) = notify(&app, Some(&key), notice_body(account, "subject", "body")).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "no contact on file must not be an error"
    );
    assert_eq!(body, json!({"delivered": false}));
    assert!(
        recorder.calls.lock().unwrap().is_empty(),
        "nothing should have been sent"
    );
}

#[tokio::test]
#[ignore]
async fn a_transport_failure_is_a_retryable_error_not_a_silent_delivered_false() {
    let pg = service().await;
    let app = app(&pg, Arc::new(FailingSender));
    let (_, key) = seed_key(&pg, "server_admin", None, Some(&[NOTICE_SCOPE])).await;
    let (account, _) = seed_key(&pg, "user", Some("transport-failure@example.com"), None).await;

    let (status, _) = notify(&app, Some(&key), notice_body(account, "subject", "body")).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
}

#[tokio::test]
#[ignore]
async fn an_unknown_account_is_a_retryable_error() {
    let pg = service().await;
    let app = app(&pg, Arc::new(RecordingSender::default()));
    let (_, key) = seed_key(&pg, "server_admin", None, Some(&[NOTICE_SCOPE])).await;

    let (status, _) = notify(
        &app,
        Some(&key),
        notice_body(Uuid::new_v4(), "subject", "body"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
}

#[tokio::test]
#[ignore]
async fn an_unauthenticated_notice_is_refused() {
    let pg = service().await;
    let app = app(&pg, Arc::new(RecordingSender::default()));

    let (status, _) = notify(&app, None, notice_body(Uuid::new_v4(), "s", "b")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = notify(
        &app,
        Some("ak_not_a_real_key"),
        notice_body(Uuid::new_v4(), "s", "b"),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
#[ignore]
async fn a_key_without_the_notice_scope_is_refused_whatever_else_it_may_do() {
    let pg = service().await;
    let app = app(&pg, Arc::new(RecordingSender::default()));
    let (account, _) = seed_key(&pg, "user", Some("scope-check@example.com"), None).await;

    let keys = [
        (
            "unrestricted admin key",
            seed_key(&pg, "server_admin", None, None).await.1,
        ),
        (
            "admin key scoped to unrestricted",
            seed_key(&pg, "server_admin", None, Some(&["unrestricted"]))
                .await
                .1,
        ),
        (
            "admin key scoped to the standing push",
            seed_key(
                &pg,
                "server_admin",
                None,
                Some(&["ethpay.server.canpushstanding"]),
            )
            .await
            .1,
        ),
        (
            "ordinary user's key",
            seed_key(&pg, "user", None, None).await.1,
        ),
        (
            "notice scope on a non-admin owner's key",
            seed_key(&pg, "user", None, Some(&[NOTICE_SCOPE])).await.1,
        ),
    ];
    for (what, key) in &keys {
        let (status, _) = notify(&app, Some(key), notice_body(account, "s", "b")).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{what}");
    }
}

#[tokio::test]
#[ignore]
async fn malformed_and_oversized_notices_are_refused() {
    let pg = service().await;
    let app = app(&pg, Arc::new(RecordingSender::default()));
    let (_, key) = seed_key(&pg, "server_admin", None, Some(&[NOTICE_SCOPE])).await;
    let account = Uuid::new_v4();

    let bad = [
        notice_body(account, "", "b"),
        notice_body(account, "   ", "b"),
        notice_body(account, &"x".repeat(201), "b"),
        notice_body(account, "s", ""),
        notice_body(account, "s", &"x".repeat(4097)),
        serde_json::to_vec(&json!({"account_id": "not-a-uuid", "subject": "s", "body": "b"}))
            .unwrap(),
        b"not json".to_vec(),
    ];
    for body in bad {
        let (status, _) = notify(&app, Some(&key), body.clone()).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{}",
            String::from_utf8_lossy(&body)
        );
    }

    let (status, _) = notify(
        &app,
        Some(&key),
        notice_body(account, "s", &"x".repeat(16 * 1024)),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
}
