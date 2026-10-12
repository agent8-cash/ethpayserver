#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `/health/chains` drives the real handler against a real Redis, because the
//! bug it guards was in how the handler read the monitor's snapshot: an absent
//! snapshot (the monitor stopped publishing and the key expired) was reported
//! as `all_healthy` and `data_fresh`. The helper's own unit test passed the
//! whole time.
//!
//! Needs `DATABASE_URL` and `TEST_REDIS_URL`; with the latter unset it returns
//! early without running anything. It writes the one shared `evmmonitor:health`
//! key, so run it with `-j 1` like the rest of the ignored suite.

use std::sync::Arc;

use async_trait::async_trait;
use axum::extract::State;
use axum::http::StatusCode;
use redis::AsyncCommands;

use auth::{Result as AuthResult, Session, SessionId, SessionService, UserInfo};
use data_service::test_support::pg_service;
use rates::NoOpRateProvider;
use server::api::extractors::MaybeAdmin;
use server::api::health::chains_health;
use server::services::RedisEVMMonitor;
use server::state::PgAppState;

const HEALTH_KEY: &str = "evmmonitor:health";

struct UnusedSessionService;

#[async_trait]
impl SessionService for UnusedSessionService {
    async fn validate_session(&self, _session_id: SessionId) -> AuthResult<(UserInfo, Session)> {
        unimplemented!("not exercised by this handler")
    }
    async fn logout(&self, _session_id: SessionId) -> AuthResult<()> {
        unimplemented!("not exercised by this handler")
    }
    async fn logout_all(&self, _session_id: SessionId) -> AuthResult<()> {
        unimplemented!("not exercised by this handler")
    }
    async fn cleanup_stale_sessions(&self) -> AuthResult<u64> {
        unimplemented!("not exercised by this handler")
    }
}

fn chain_json(is_healthy: bool) -> String {
    format!(
        r#"{{"chain_id":11155111,"chain_name":"Sepolia","status":"connected",
        "current_block":100,"last_processed_block":100,"watched_addresses":0,
        "is_healthy":{is_healthy}}}"#
    )
}

/// Publish `snapshot` as the monitor would (`None` = the key has expired), then
/// call the handler and return what a public caller sees.
async fn ask(snapshot: Option<&str>) -> Option<(StatusCode, bool, bool, usize)> {
    let pg = Arc::new(pg_service().await);
    let redis_url = std::env::var("TEST_REDIS_URL").ok()?;
    let monitor = RedisEVMMonitor::connect(&redis_url)
        .await
        .unwrap_or_else(|e| panic!("TEST_REDIS_URL is set but connecting failed: {e}"));

    let client = redis::Client::open(redis_url.as_str()).expect("redis client");
    let mut conn = client
        .get_multiplexed_async_connection()
        .await
        .expect("redis connection");
    match snapshot {
        Some(json) => conn.set::<_, _, ()>(HEALTH_KEY, json).await.unwrap(),
        None => conn.del::<_, ()>(HEALTH_KEY).await.unwrap(),
    }

    let state = PgAppState::new(
        pg,
        Arc::new(UnusedSessionService),
        Some(Arc::new(monitor)),
        Arc::new(NoOpRateProvider),
        Arc::new(server::services::email::NoopEmailSender),
    );
    let (status, body) = chains_health(MaybeAdmin(false), State(state)).await;
    Some((status, body.all_healthy, body.data_fresh, body.chains.len()))
}

#[tokio::test]
#[ignore]
async fn a_monitor_that_published_nothing_is_not_reported_healthy_or_fresh() {
    let Some((status, all_healthy, data_fresh, chains)) = ask(None).await else {
        return;
    };
    assert_eq!(chains, 0);
    assert!(!all_healthy, "no data must not read as all healthy");
    assert!(!data_fresh, "no data must not read as fresh");
    // 200, deliberately. The body is the answer here, and a non-2xx costs every
    // caller that treats it as a failed request the body that explains why - the
    // client's network panel reads `data_fresh` to render exactly this state, and
    // an environment running no monitor at all sees its normal state. A 503 here
    // made scout.spec.ts fail on eleven of these and took testnet red.
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
#[ignore]
async fn an_empty_published_list_is_not_reported_healthy_or_fresh() {
    let Some((status, all_healthy, data_fresh, _)) = ask(Some("[]")).await else {
        return;
    };
    assert!(!all_healthy);
    assert!(!data_fresh);
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
#[ignore]
async fn a_healthy_snapshot_is_reported_healthy_and_fresh() {
    let json = format!("[{}]", chain_json(true));
    let Some((status, all_healthy, data_fresh, chains)) = ask(Some(&json)).await else {
        return;
    };
    assert_eq!(status, StatusCode::OK);
    assert_eq!(chains, 1);
    assert!(all_healthy);
    assert!(data_fresh);
}

#[tokio::test]
#[ignore]
async fn an_unhealthy_chain_is_not_reported_all_healthy() {
    let json = format!("[{}]", chain_json(false));
    let Some((status, all_healthy, data_fresh, _)) = ask(Some(&json)).await else {
        return;
    };
    assert_eq!(status, StatusCode::OK);
    assert!(!all_healthy);
    assert!(!data_fresh);
}
