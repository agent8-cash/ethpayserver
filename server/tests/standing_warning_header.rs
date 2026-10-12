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
use server::api::standing_warning::{X_ACCOUNT_CHECKOUT_URL, X_ACCOUNT_WARNING};
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
    hold_with_route(pg, account, days_left, Some("https://pay.example/c")).await;
}

/// `route: None` is the case the entitlement API permits and the warning must still
/// report: `checkout_url` is `Option` where billing sets it, so a merchant warned with
/// nowhere to go is reachable in production rather than hypothetical.
async fn hold_with_route(pg: &PgDataService, account: Uuid, days_left: i64, route: Option<&str>) {
    pg.apply_account_standing(&AccountStanding {
        account_id: account,
        version: 1,
        in_good_standing: true,
        paid_through: Some(Utc::now() + Duration::days(days_left)),
        plan_name: "p".into(),
        checkout_url: route.map(str::to_string),
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
        // THE ROUTE REACHES THE WIRE AS ITS OWN HEADER. The struct field proves
        // nothing about what a caller receives; this is the hop between them.
        assert_eq!(
            response
                .headers()
                .get(&X_ACCOUNT_CHECKOUT_URL)
                .expect("checkout url header present when a route exists")
                .to_str()
                .unwrap(),
            "https://pay.example/c"
        );
    }
}

/// RE-POINTED. This asserted that a routeless account still got a warning, which is
/// the behaviour the rule forbids - and it passed, so it could not flag the gap. The
/// reviewer caught it locking in a divergence.
///
/// It now asserts the rule: a warning carrying neither an invoice url nor a renewal
/// action is NOT DELIVERED. No warning header, no checkout header, nothing. There is
/// no renewal endpoint to name yet, so silence is the only honest answer for an
/// account with no open invoice - which under on-demand issuance is most of them.
#[tokio::test]
#[ignore]
async fn a_caller_with_no_means_to_act_gets_no_warning_at_all() {
    let pg = Arc::new(pg_service().await);
    let (user, key, _) = seed(&pg).await;
    hold_with_route(&pg, user, 2, None).await;
    let response = get(&app(&pg), &key).await;

    assert!(
        response.headers().get(&X_ACCOUNT_WARNING).is_none(),
        "a warning carrying no means to act must not be delivered: an unactionable \
         warning is later read as evidence the merchant could have acted"
    );
    assert!(
        response.headers().get(&X_ACCOUNT_CHECKOUT_URL).is_none(),
        "and no route header either, since there is no route"
    );
}

/// The allow-list on the WIRE, not only on the struct. Acceptance item 3 asks that a
/// field added later fails by default; this pins the full set of x-account-* headers a
/// response may carry, so a new one has to be added here deliberately.
#[tokio::test]
#[ignore]
async fn the_response_carries_no_account_headers_beyond_the_warning_and_its_route() {
    let pg = Arc::new(pg_service().await);
    let (user, key, _) = seed(&pg).await;
    hold(&pg, user, 2).await;
    let response = get(&app(&pg), &key).await;

    let account_headers: Vec<String> = response
        .headers()
        .keys()
        .map(|k| k.as_str().to_ascii_lowercase())
        .filter(|k| k.starts_with("x-account"))
        .collect();
    let mut sorted = account_headers.clone();
    sorted.sort();
    assert_eq!(
        sorted,
        vec![
            "x-account-checkout-url".to_string(),
            "x-account-warning".to_string()
        ],
        "only the expiry fact and the means to act may reach the wire; a new \
         x-account-* header must be added to this list deliberately"
    );

    // And nothing recognisable as profiling in the value we do send.
    let text = response
        .headers()
        .get(&X_ACCOUNT_WARNING)
        .unwrap()
        .to_str()
        .unwrap()
        .to_ascii_lowercase();
    for forbidden in [
        "plan", "tier", "usd", "$", "invoice", "paid", "days ago", "lapsed",
    ] {
        assert!(
            !text.contains(forbidden),
            "payload discloses {forbidden:?}: {text}"
        );
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
