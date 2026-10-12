//! HTTP handlers for liveness, readiness, basic health, chains health and metrics.

use auth::SessionService;
use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};

use crate::api::AdminAuth;
use crate::api::extractors::MaybeAdmin;
use crate::metrics;
use crate::services::EVMMonitor;
use crate::state::PgAppState;

use super::PROBE_TIMEOUT;
use super::models::{ChainHealthInfo, ChainsHealthResponse, HealthResponse, ReadinessResponse};

/// Health check endpoint.
///
/// Returns the service health status including database connectivity.
#[utoipa::path(
    get,
    path = "/health",
    tag = "health",
    responses(
        (status = 200, description = "Service is healthy", body = HealthResponse),
        (status = 503, description = "Service is unhealthy", body = HealthResponse),
    )
)]
pub async fn health_check<A>(
    State(state): State<PgAppState<A>>,
) -> (StatusCode, Json<HealthResponse>)
where
    A: Send + Sync + 'static,
{
    let db_healthy = state.data_service.health_check().await.is_ok();

    // Check Redis health if EVM monitor is configured
    let redis_healthy = if let Some(ref monitor) = state.evm_monitor {
        Some(monitor.health_check().await.is_ok())
    } else {
        None
    };

    // Service is healthy if DB is healthy (Redis is optional)
    let all_healthy = db_healthy && redis_healthy.unwrap_or(true);

    let response = HealthResponse {
        status: if all_healthy {
            "healthy".to_string()
        } else {
            "unhealthy".to_string()
        },
        version: env!("CARGO_PKG_VERSION").to_string(),
        build_sha: env!("ETHPAYSERVER_BUILD_SHA").to_string(),
        database: db_healthy,
        redis: redis_healthy,
    };

    let status = if all_healthy {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };

    (status, Json(response))
}

/// Simple liveness probe.
///
/// Always returns 200 OK if the server is running.
#[utoipa::path(
    get,
    path = "/health/live",
    tag = "health",
    responses(
        (status = 200, description = "Service is alive"),
    )
)]
pub async fn liveness() -> StatusCode {
    StatusCode::OK
}

/// Readiness probe.
///
/// Returns 200 if the service is ready to accept traffic (Postgres, Redis,
/// and every configured RPC chain respond within 1 s). Returns 503 with a
/// JSON body listing the failing dependencies otherwise.
#[utoipa::path(
    get,
    path = "/health/ready",
    tag = "health",
    responses(
        (status = 200, description = "Service is ready", body = ReadinessResponse),
        (status = 503, description = "Service is not ready", body = ReadinessResponse),
    )
)]
pub async fn readiness<A>(
    State(state): State<PgAppState<A>>,
) -> (StatusCode, Json<ReadinessResponse>)
where
    A: Send + Sync + 'static,
{
    let mut failing = Vec::new();

    // Check Postgres with timeout
    let db_ok = tokio::time::timeout(PROBE_TIMEOUT, state.data_service.health_check())
        .await
        .map(|r| r.is_ok())
        .unwrap_or(false);
    if !db_ok {
        failing.push("postgres".to_string());
    }

    // Check Redis + RPC chains if EVM monitor is configured
    if let Some(ref monitor) = state.evm_monitor {
        let redis_ok = tokio::time::timeout(PROBE_TIMEOUT, monitor.health_check())
            .await
            .map(|r| r.is_ok())
            .unwrap_or(false);
        if !redis_ok {
            failing.push("redis".to_string());
        }

        // Check RPC chain health (read from Redis, published by evmmonitor)
        if redis_ok
            && let Ok(Ok(chains)) =
                tokio::time::timeout(PROBE_TIMEOUT, monitor.get_chain_health()).await
        {
            for chain in &chains {
                if !chain.is_healthy {
                    failing.push(format!("rpc:{}", chain.chain_id));
                }
            }
        }
    }

    if failing.is_empty() {
        (
            StatusCode::OK,
            Json(ReadinessResponse {
                status: "ready".to_string(),
                failing: vec![],
            }),
        )
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ReadinessResponse {
                status: "not_ready".to_string(),
                failing,
            }),
        )
    }
}

/// Get health status for all monitored chains.
///
/// Returns per-chain health information from the evmmonitor service.
/// Health data is published to Redis by evmmonitor every 10 seconds.
///
/// Requires server admin authentication.
#[utoipa::path(
    get,
    path = "/health/chains",
    tag = "health",
    responses(
        (status = 200, description = "Chain health information. Block heights, \
            watched-address counts and failure reasons are included only for \
            server admins; everyone else gets chain identity and up/down. \
            A monitor that has published no snapshot recently also answers 200, \
            with an empty list and both flags false - read `data_fresh` rather \
            than the status code to tell that case from a healthy one.",
            body = ChainsHealthResponse),
        (status = 503, description = "No chain monitor is configured on this server"),
    )
)]
pub async fn chains_health<A>(
    MaybeAdmin(is_admin): MaybeAdmin,
    State(state): State<PgAppState<A>>,
) -> (StatusCode, Json<ChainsHealthResponse>)
where
    A: SessionService + 'static,
{
    // Get Redis connection from EVM monitor
    let Some(ref monitor) = state.evm_monitor else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ChainsHealthResponse {
                chains: vec![],
                all_healthy: false,
                data_fresh: false,
            }),
        );
    };

    // Read health data from Redis
    match monitor.get_chain_health().await {
        Ok(chains) => {
            // The monitor republishes its snapshot every few seconds under a key
            // that expires, so an empty list means it has stopped publishing,
            // not that every chain is well. `all()` alone is vacuously true on it.
            //
            // That empty case answers 200 with both flags false, NOT 503. The
            // body already says "we know nothing", and saying it twice costs
            // more than it buys: every caller that treats a non-2xx as a failed
            // request loses the body that explains why. The client's network
            // panel reads `data_fresh` to render its stale state, so a 503 turns
            // the one case that branch exists for into a generic request error -
            // and an environment with no monitor running at all (e2e) sees its
            // normal state as a hard failure. The 503 above is different: there
            // the monitor is not configured, so there is no body worth reading.
            let fresh = super::deep::chains_are_fresh(&chains);

            // Everyone may know a chain is down; only an admin may know how far
            // behind it is or which endpoint failed.
            let chain_infos: Vec<ChainHealthInfo> = chains
                .into_iter()
                .map(super::models::chain_health_info)
                .map(|info| if is_admin { info } else { info.redact() })
                .collect();

            (
                StatusCode::OK,
                Json(ChainsHealthResponse {
                    chains: chain_infos,
                    all_healthy: fresh,
                    data_fresh: fresh,
                }),
            )
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to get chain health from Redis");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ChainsHealthResponse {
                    chains: vec![],
                    all_healthy: false,
                    data_fresh: false,
                }),
            )
        }
    }
}

/// Prometheus metrics endpoint.
///
/// Returns metrics in Prometheus exposition format for scraping.
///
/// Requires server admin authentication.
#[utoipa::path(
    get,
    path = "/metrics",
    tag = "health",
    responses(
        (status = 200, description = "Prometheus metrics"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Admin access required"),
        (status = 503, description = "Metrics not available"),
    ),
    security(("bearer_auth" = []))
)]
pub async fn prometheus_metrics<A>(
    _admin: AdminAuth,
    State(state): State<PgAppState<A>>,
) -> impl IntoResponse
where
    A: SessionService + 'static,
{
    // Update user and store count gauges before rendering
    if let Ok(count) = state.data_service.count_users().await {
        metrics::set_registered_users(count as u64);
    }
    if let Ok(count) = state.data_service.count_stores().await {
        metrics::set_stores(count as u64);
    }

    // Scrape DB pool stats
    let pool = state.data_service.pool();
    let pool_size = pool.size() as u64;
    let pool_idle = pool.num_idle() as u64;
    metrics::set_db_pool_connections("idle", pool_idle);
    metrics::set_db_pool_connections("used", pool_size.saturating_sub(pool_idle));

    match metrics::render() {
        Some(body) => (
            StatusCode::OK,
            [("content-type", "text/plain; version=0.0.4; charset=utf-8")],
            body,
        ),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            [("content-type", "text/plain")],
            "Metrics not initialized".to_string(),
        ),
    }
}
