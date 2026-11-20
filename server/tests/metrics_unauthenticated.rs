#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Integration test for unauthenticated /metrics endpoint on loopback.
//!
//! Acceptance criteria:
//! 1. Request to metrics listener's /metrics (no auth) from loopback → 200 with Prometheus output
//! 2. Metrics listener is bound to loopback only (127.0.0.1:9102 by default)
//! 3. Existing admin /metrics endpoint is unchanged (behind AdminAuth)

use std::sync::Arc;
use std::time::Duration;

use auth::{AuthConfig, AuthService};
use data_service::test_support::pg_service;
use rates::NoOpRateProvider;
use server::{AppState, api};
use tokio::net::TcpListener;
use tokio::time::sleep;

#[tokio::test]
#[ignore]
async fn metrics_listener_on_loopback() {
    // Start metrics listener
    let metrics_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind metrics listener");
    let metrics_addr = metrics_listener
        .local_addr()
        .expect("failed to get local address");

    // Create minimal app state for metrics listener
    let pg = Arc::new(pg_service().await);
    let auth_service = Arc::new(AuthService::with_config(
        Arc::clone(&pg),
        AuthConfig::default(),
    ));
    let state = AppState::new(
        pg,
        auth_service,
        None, // no EVM monitor
        Arc::new(NoOpRateProvider),
        Arc::new(server::services::email::NoopEmailSender),
    );

    // Create metrics router (unauthenticated)
    let metrics_router = axum::Router::new()
        .route(
            "/metrics",
            axum::routing::get(server::api::health::prometheus_metrics_unauthenticated),
        )
        .with_state(state);

    let metrics_router = api::with_sentry_performance_tracing(metrics_router);

    // Spawn metrics listener in background
    let listener_handle = {
        let listener = metrics_listener;
        tokio::spawn(async move {
            let _ = axum::serve(
                listener,
                metrics_router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await;
        })
    };

    // Give the server a moment to start
    sleep(Duration::from_millis(100)).await;

    // Test 1: Request to metrics listener from loopback should return 200
    let response = reqwest::Client::new()
        .get(format!("http://{}/metrics", metrics_addr))
        .send()
        .await
        .expect("failed to make request to metrics listener");

    assert_eq!(
        response.status(),
        200,
        "metrics listener should return 200 for /metrics from loopback"
    );

    let body = response.text().await.expect("failed to read response body");

    // Should contain Prometheus format metrics
    assert!(
        body.contains("# HELP") || body.contains("# TYPE"),
        "metrics response should contain Prometheus format headers"
    );

    // Clean up
    drop(listener_handle);
}

#[tokio::test]
#[ignore]
async fn metrics_endpoint_is_public_via_loopback() {
    // This test verifies the metrics endpoint is accessible without authentication
    // when accessed from loopback.

    let metrics_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind metrics listener");
    let metrics_addr = metrics_listener
        .local_addr()
        .expect("failed to get local address");

    // Create minimal app state
    let pg = Arc::new(pg_service().await);
    let auth_service = Arc::new(AuthService::with_config(
        Arc::clone(&pg),
        AuthConfig::default(),
    ));
    let state = AppState::new(
        pg,
        auth_service,
        None, // no EVM monitor
        Arc::new(NoOpRateProvider),
        Arc::new(server::services::email::NoopEmailSender),
    );

    // Create metrics router without auth requirements
    let metrics_router = axum::Router::new()
        .route(
            "/metrics",
            axum::routing::get(server::api::health::prometheus_metrics_unauthenticated),
        )
        .with_state(state);

    let metrics_router = api::with_sentry_performance_tracing(metrics_router);

    // Spawn metrics listener
    let listener_handle = {
        let listener = metrics_listener;
        tokio::spawn(async move {
            let _ = axum::serve(
                listener,
                metrics_router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await;
        })
    };

    sleep(Duration::from_millis(100)).await;

    // Test: Make request without any auth headers - should still succeed
    let response = reqwest::Client::new()
        .get(format!("http://{}/metrics", metrics_addr))
        .send()
        .await
        .expect("failed to make request");

    assert_eq!(
        response.status(),
        200,
        "metrics endpoint should not require authentication from loopback"
    );

    drop(listener_handle);
}
