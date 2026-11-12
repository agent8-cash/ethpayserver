#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Metrics exposure, driven through the shipped routers against a real Postgres.
//!
//! - `loopback_metrics_listener_serves_without_credentials`: the loopback
//!   listener (`api::metrics_router`, the same function `server.rs` serves on
//!   `METRICS_HOST:METRICS_PORT`) answers a real TCP request with no
//!   `Authorization` header: 200 and Prometheus text.
//! - `public_router_metrics_still_requires_admin`: the public router
//!   (`api::router`) still gates `/metrics` behind `AdminAuth`. Without that
//!   gate, business counts would be published to anyone who reaches the API.
//!
//! Needs `DATABASE_URL`; runs in CI's `--run-ignored` step.

use std::sync::{Arc, Once};
use std::time::Duration;

use auth::{AuthConfig, AuthService};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use data_service::test_support::pg_service;
use rates::NoOpRateProvider;
use server::state::PgAppState;
use server::{AppState, api};
use tokio::net::TcpListener;
use tokio::time::sleep;
use tower::ServiceExt;

/// `init_metrics` installs a process-global recorder and refuses to run
/// without a Sentry client bound, so both tests share one init. Each
/// integration file is its own process, so this cannot clash with other files.
fn ensure_metrics_initialised() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        // Keep the guard alive for the process; dropping it would unbind the client.
        std::mem::forget(sentry::init(sentry::ClientOptions::default()));
        server::metrics::init_metrics().expect("init_metrics");
    });
}

async fn app_state() -> PgAppState<AuthService<data_service::PgDataService>> {
    let pg = Arc::new(pg_service().await);
    let auth_service = Arc::new(AuthService::with_config(
        Arc::clone(&pg),
        AuthConfig::default(),
    ));
    AppState::new(
        pg,
        auth_service,
        None, // no EVM monitor
        Arc::new(NoOpRateProvider),
        Arc::new(server::services::email::NoopEmailSender),
    )
}

#[tokio::test]
#[ignore]
async fn loopback_metrics_listener_serves_without_credentials() {
    ensure_metrics_initialised();
    let state = app_state().await;

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback metrics listener");
    let addr = listener.local_addr().expect("local addr");
    assert!(addr.ip().is_loopback(), "metrics listener must be loopback");

    let router = api::metrics_router(state);
    let server = tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await;
    });
    sleep(Duration::from_millis(100)).await;

    let response = reqwest::Client::new()
        .get(format!("http://{addr}/metrics"))
        .send()
        .await
        .expect("request to loopback metrics listener");

    let status = response.status();
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let body = response.text().await.expect("read body");
    server.abort();

    assert_eq!(
        status,
        StatusCode::OK,
        "no-credential GET must succeed: {body}"
    );
    assert!(
        content_type.starts_with("text/plain; version=0.0.4"),
        "unexpected content-type: {content_type}"
    );
    assert!(
        body.contains("# TYPE ethpayserver_registered_users"),
        "body is not Prometheus exposition: {body}"
    );
}

#[tokio::test]
#[ignore]
async fn public_router_metrics_still_requires_admin() {
    ensure_metrics_initialised();
    let state = app_state().await;

    let response = api::router(state, false, None, None, None)
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("oneshot");

    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let body = String::from_utf8_lossy(&bytes);

    assert!(
        matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN),
        "public /metrics without admin auth must be 401/403, got {status}"
    );
    assert!(
        !body.contains("ethpayserver_"),
        "public /metrics leaked metric output without auth"
    );
}
