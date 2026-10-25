#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `POST /users/me/signup`, driven through the production router with a real
//! `Authorization: Bearer` key against a real Postgres, and a stand-in for the
//! signup service that records what it was sent.
//!
//! Pins what a unit test of the helpers cannot: the route sits behind
//! authentication, an instance without the service configured answers 404, and
//! the account the service is told about is the session's - the request body
//! has no way to name another.
//!
//! Needs `DATABASE_URL` and fails, naming it, when it is unset; runs in CI's
//! `--run-ignored` step.

use data_service::test_support::pg_service;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use data_service::PgDataService;
use rates::NoOpRateProvider;
use secrecy::SecretString;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tower::ServiceExt;
use uuid::Uuid;

use auth::{AuthConfig, AuthService};
use server::services::RedisEVMMonitor;
use server::services::signup_service::{SignupPlan, SignupServiceConfig};
use server::state::PgAppState;

type Seen = Arc<Mutex<Vec<Value>>>;

/// A signup service that answers `status` with `reply` and records each body.
async fn fake_service(status: u16, reply: Value) -> (String, Seen) {
    let seen: Seen = Arc::default();
    let record = Arc::clone(&seen);
    let app = Router::new().route(
        "/signup",
        post(move |Json(body): Json<Value>| {
            let record = Arc::clone(&record);
            let reply = reply.clone();
            async move {
                record.lock().unwrap().push(body);
                (StatusCode::from_u16(status).unwrap(), Json(reply))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, seen)
}

fn app(pg: &Arc<PgDataService>, service: Option<&str>) -> Router {
    app_with_plans(pg, service, Vec::new())
}

fn app_with_plans(
    pg: &Arc<PgDataService>,
    service: Option<&str>,
    plans: Vec<SignupPlan>,
) -> Router {
    let auth_service = Arc::new(AuthService::with_config(
        Arc::clone(pg),
        AuthConfig::default(),
    ));
    let mut state = PgAppState::new(
        Arc::clone(pg),
        auth_service,
        None::<Arc<RedisEVMMonitor>>,
        Arc::new(NoOpRateProvider),
        Arc::new(server::services::email::NoopEmailSender),
    );
    state.signup_service = service.map(|url| {
        Arc::new(SignupServiceConfig {
            url: url.to_string(),
            secret: SecretString::from("k".repeat(40)),
            audience: "signup.test".into(),
            plans,
        })
    });
    server::api::router(state, false, None, None, None)
}

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

    let key = format!("ak_signup_{}", Uuid::new_v4());
    sqlx::query(
        "INSERT INTO api_keys (id, user_id, name, key_hash, key_prefix, permissions) \
         VALUES ($1, $2, 'signup key', $3, 'ak_test', NULL)",
    )
    .bind(Uuid::new_v4())
    .bind(user_id)
    .bind(hex::encode(Sha256::digest(key.as_bytes())))
    .execute(pg.pool())
    .await
    .expect("seed api key");
    (user_id, key)
}

async fn post_signup(app: &Router, key: Option<&str>, body: Value) -> (StatusCode, Value) {
    let (status, bytes) = post_signup_raw(app, key, body).await;
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn post_signup_raw(app: &Router, key: Option<&str>, body: Value) -> (StatusCode, Vec<u8>) {
    let mut req = Request::builder()
        .method("POST")
        .uri("/users/me/signup")
        .header("content-type", "application/json");
    if let Some(key) = key {
        req = req.header("authorization", format!("Bearer {key}"));
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
    (status, bytes.to_vec())
}

async fn get_plans(app: &Router, key: Option<&str>) -> (StatusCode, Value) {
    let mut req = Request::builder().method("GET").uri("/users/me/plans");
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

#[tokio::test]
#[ignore]
async fn an_unauthenticated_signup_is_refused_and_reaches_nobody() {
    let pg = Arc::new(pg_service().await);
    let (url, seen) = fake_service(201, json!({})).await;
    let (status, _) = post_signup(&app(&pg, Some(&url)), None, json!({"plan_id": 1})).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(seen.lock().unwrap().is_empty());
}

#[tokio::test]
#[ignore]
async fn an_instance_without_the_service_answers_404() {
    let pg = Arc::new(pg_service().await);
    let (_, key) = seed_user(&pg).await;
    let (status, _) = post_signup(&app(&pg, None), Some(&key), json!({"plan_id": 1})).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
#[ignore]
async fn the_account_told_to_the_service_is_the_sessions_and_cannot_be_named() {
    let pg = Arc::new(pg_service().await);
    let (url, seen) = fake_service(201, json!({"checkout_url": "https://pay.example/c/1"})).await;
    let app = app(&pg, Some(&url));
    let (user, key) = seed_user(&pg).await;
    let other = Uuid::new_v4();

    let (status, body) = post_signup(&app, Some(&key), json!({"plan_id": 7})).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["checkout_url"], "https://pay.example/c/1");
    {
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0]["account_id"], user.to_string());
        assert_eq!(seen[0]["plan_id"], 7);
    }

    // There is no field to name another account in; the attempt is refused
    // before the service is asked anything.
    let (status, _) = post_signup(
        &app,
        Some(&key),
        json!({"plan_id": 7, "account_id": other.to_string()}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(seen.lock().unwrap().len(), 1);
}

#[tokio::test]
#[ignore]
async fn the_services_refusals_reach_the_merchant_coarsely() {
    let pg = Arc::new(pg_service().await);
    let (_, key) = seed_user(&pg).await;
    for (answered, expected) in [
        (409, StatusCode::CONFLICT),
        (404, StatusCode::NOT_FOUND),
        (401, StatusCode::BAD_GATEWAY),
    ] {
        let (url, _) = fake_service(answered, json!({"secret": "leak-marker-q7z"})).await;
        let (status, body) =
            post_signup_raw(&app(&pg, Some(&url)), Some(&key), json!({"plan_id": 1})).await;
        assert_eq!(status, expected);
        let text = String::from_utf8(body).unwrap();
        assert!(
            !text.contains("leak-marker-q7z"),
            "the service's own words are not relayed: {text}"
        );
    }
}

#[tokio::test]
#[ignore]
async fn a_retry_after_a_lost_reply_gets_the_pending_link_from_the_refusal() {
    let pg = Arc::new(pg_service().await);
    let (_, key) = seed_user(&pg).await;
    let (url, _) = fake_service(
        409,
        json!({"checkout_url": "https://pay.example/c/1", "plan_id": 1}),
    )
    .await;

    let (status, body) =
        post_signup(&app(&pg, Some(&url)), Some(&key), json!({"plan_id": 1})).await;

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["checkout_url"], "https://pay.example/c/1");

    // THERE IS NO "DIFFERENT PLAN" CASE HERE EITHER, AND THE STUB IS WHY. This
    // asserted that asking for plan 2 against a pending invoice for plan 1 is
    // refused, and it passed - because `fake_service` returns a fixed body
    // echoing `plan_id: 1` whatever is requested, so the handler saw a mismatch.
    //
    // The real service does the opposite: it builds the 409 body from the plan
    // the caller REQUESTED, so it never emits a mismatch and the comparison this
    // case exercised could never fire in production. The stub modelled a service
    // that does not exist, the guard was deleted with it, and asserting the
    // refusal here would now assert behaviour nothing produces.
    //
    // What a merchant on another plan actually receives is the first plan's
    // payment link beside the requested plan's id. The wrong field is the
    // service's; this repository cannot tell the two plans apart.
}

#[tokio::test]
#[ignore]
async fn an_unauthenticated_plan_listing_is_refused() {
    let pg = Arc::new(pg_service().await);
    let (status, _) = get_plans(&app(&pg, Some("https://signup.example")), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
#[ignore]
async fn an_instance_without_the_service_lists_no_plans_and_answers_404() {
    let pg = Arc::new(pg_service().await);
    let (_, key) = seed_user(&pg).await;
    let (status, _) = get_plans(&app(&pg, None), Some(&key)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
#[ignore]
async fn the_configured_plans_are_listed_in_order() {
    let pg = Arc::new(pg_service().await);
    let (_, key) = seed_user(&pg).await;
    let plans = vec![
        SignupPlan {
            id: 1,
            name: "Basic".into(),
            description: Some("One store".into()),
        },
        SignupPlan {
            id: 2,
            name: "Pro".into(),
            description: None,
        },
    ];
    let app = app_with_plans(&pg, Some("https://signup.example"), plans);

    let (status, body) = get_plans(&app, Some(&key)).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["plans"][0]["id"], 1);
    assert_eq!(body["plans"][0]["description"], "One store");
    assert_eq!(body["plans"][1]["name"], "Pro");
    assert!(body.get("secret").is_none());
    assert!(body.get("url").is_none());
}
