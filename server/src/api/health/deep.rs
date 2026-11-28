//! Deep health diagnostic endpoint and its dependency probes.

use std::collections::HashMap;

use axum::{
    Json,
    extract::State,
    http::{HeaderMap, HeaderValue, StatusCode},
};
use evm::monitor::{ChainHealth, SourceStatus};
use tokio::time::Instant;

use crate::services::{EVMMonitor, RedisEVMMonitor, reconcile_watches};
use crate::state::PgAppState;

use super::PROBE_TIMEOUT;
use super::models::{
    DeepHealthResponse, DependencyHealth, MonitorHealth, RpcHealth, WatchReconciliationHealth,
};

/// Deep health diagnostic endpoint.
///
/// Returns per-dependency status with latencies. Not tied to load-balancer
/// decisions; intended for operators and dashboards. No authentication required.
///
/// The response body carries one field beyond what its declared schema
/// documents: `watch_reconciliation` (see [`WatchReconciliationHealth`]). It
/// is not part of `api_types::DeepHealthResponse` and so is not part of the
/// `body` schema below - see that type's docs for why.
#[utoipa::path(
    get,
    path = "/health/deep",
    tag = "health",
    responses(
        (status = 200, description = "Deep health diagnostic", body = DeepHealthResponse),
    )
)]
#[allow(
    clippy::too_many_lines,
    clippy::cognitive_complexity,
    reason = "deep health probe: timed Postgres + Redis + per-chain RPC + watch-reconciliation checks, each with its own error mapping; splitting would obscure the HTTP response shape"
)]
pub async fn deep_health<A>(
    State(state): State<PgAppState<A>>,
) -> (StatusCode, HeaderMap, Json<DeepHealthResponseWire>)
where
    A: Send + Sync + 'static,
{
    let postgres = probe_dependency(state.data_service.health_check()).await;

    let (redis, rpcs, monitor, evmmonitor_sentry_release, watch_reconciliation) =
        match state.evm_monitor.as_ref() {
            Some(evm_monitor) => probe_evm_monitor(evm_monitor, &state.data_service).await,
            None => (
                DependencyHealth {
                    status: "ok".to_string(),
                    latency_ms: 0,
                    error: Some("not configured".to_string()),
                },
                HashMap::new(),
                MonitorHealth {
                    status: "ok".to_string(),
                    data_fresh: false,
                },
                None,
                WatchReconciliationHealth::unknown("no monitor configured"),
            ),
        };

    let response = DeepHealthResponse {
        build_sha: env!("ETHPAYSERVER_BUILD_SHA").to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        postgres,
        redis,
        rpcs,
        monitor,
        // The relying party this process actually resolved at startup, not
        // whatever the environment says now.
        webauthn: state.webauthn.clone(),
    };

    (
        StatusCode::OK,
        sentry_release_headers(option_env!("SENTRY_RELEASE"), evmmonitor_sentry_release),
        Json(DeepHealthResponseWire {
            base: response,
            watch_reconciliation,
        }),
    )
}

/// The wire shape of `/health/deep`: the shared `DeepHealthResponse` plus
/// `watch_reconciliation` flattened alongside it - see the handler doc
/// comment for why that field cannot live on `DeepHealthResponse` itself
/// this side of a `payserver-commons` pin bump.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DeepHealthResponseWire {
    #[serde(flatten)]
    base: DeepHealthResponse,
    watch_reconciliation: WatchReconciliationHealth,
}

/// Value for the `x-sentry-release` header. Split out from [`deep_health`]
/// so the empty-vs-present behaviour is unit-testable without booting a
/// server - `option_env!` itself resolves at compile time and can't be
/// varied from a test.
pub(super) fn sentry_release_header(compiled: Option<&'static str>) -> &'static str {
    compiled.unwrap_or_default()
}

/// Build the `/health/deep` response headers carrying both processes'
/// compiled `SENTRY_RELEASE`. Split out from [`deep_health`] so the
/// presence/absence behaviour is unit-testable without booting a server.
///
/// `SENTRY_RELEASE` and `ETHPAYSERVER_BUILD_SHA` are set by two separate CI
/// steps from the same commit sha, so they can drift apart without either
/// build step failing - a later stage that rebuilds from source without
/// re-exporting `SENTRY_RELEASE` would ship a binary with a correct
/// `build_sha` and an empty Sentry release, silently. Putting the compiled
/// value on the response as a header (rather than trusting the build log)
/// lets a deploy check compare it against `build_sha` from the same running
/// process.
///
/// evmmonitor is a second binary that tags its own Sentry events from the
/// same `SENTRY_RELEASE`, compiled in a separate CI build step from the same
/// commit sha - and its only HTTP surface (a loopback Prometheus
/// `/metrics` for RPC-volume benchmarking) has no way to expose a value
/// like this one directly. Its release is relayed here from the health
/// channel it already reports
/// through, rather than trusted from the build that produced it. Absent (not
/// configured, or not yet observed) rather than an empty string, so a deploy
/// check can tell "no evmmonitor to check" from "evmmonitor reported an
/// empty release".
pub(super) fn sentry_release_headers(
    compiled: Option<&'static str>,
    evmmonitor_release: Option<String>,
) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        "x-sentry-release",
        HeaderValue::from_static(sentry_release_header(compiled)),
    );
    if let Some(release) = evmmonitor_release
        && let Ok(value) = HeaderValue::from_str(&release)
    {
        headers.insert("x-evmmonitor-sentry-release", value);
    }
    headers
}

/// Timeout a health-check future and convert the outcome into `DependencyHealth`.
async fn probe_dependency<F, T, E>(fut: F) -> DependencyHealth
where
    F: std::future::Future<Output = Result<T, E>>,
    E: std::fmt::Display,
{
    let start = Instant::now();
    let result = tokio::time::timeout(PROBE_TIMEOUT, fut).await;
    let latency_ms = start.elapsed().as_millis() as u64;
    match result {
        Ok(Ok(_)) => DependencyHealth {
            status: "ok".to_string(),
            latency_ms,
            error: None,
        },
        Ok(Err(e)) => DependencyHealth {
            status: "error".to_string(),
            latency_ms,
            error: Some(e.to_string()),
        },
        Err(_) => DependencyHealth {
            status: "error".to_string(),
            latency_ms,
            error: Some("timeout".to_string()),
        },
    }
}

/// Probe EVM-monitor-backed dependencies (Redis health + per-chain RPC health +
/// monitor freshness + evmmonitor's own Sentry release + watch reconciliation)
/// and assemble the sub-sections of `DeepHealthResponse` plus the
/// `x-evmmonitor-sentry-release` header value.
async fn probe_evm_monitor(
    evm_monitor: &RedisEVMMonitor,
    data_service: &data_service::PgDataService,
) -> (
    DependencyHealth,
    HashMap<String, RpcHealth>,
    MonitorHealth,
    Option<String>,
    WatchReconciliationHealth,
) {
    let redis = probe_dependency(evm_monitor.health_check()).await;

    let chains_start = Instant::now();
    let chains_result = tokio::time::timeout(PROBE_TIMEOUT, evm_monitor.get_chain_health()).await;
    let chains_latency = chains_start.elapsed().as_millis() as u64;

    let (rpcs, data_fresh) = match chains_result {
        Ok(Ok(chains)) => {
            let fresh = chains_are_fresh(&chains);
            (build_rpc_map(chains, chains_latency), fresh)
        }
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "deep health: failed to get chain health");
            (HashMap::new(), false)
        }
        Err(_) => {
            tracing::warn!("deep health: chain health fetch timed out");
            (HashMap::new(), false)
        }
    };

    let monitor = MonitorHealth {
        status: if data_fresh { "ok" } else { "error" }.to_string(),
        data_fresh,
    };

    let sentry_release = probe_evmmonitor_sentry_release(evm_monitor).await;
    let watch_reconciliation = probe_watch_reconciliation(evm_monitor, data_service).await;

    (redis, rpcs, monitor, sentry_release, watch_reconciliation)
}

/// Diff the monitor's actual watch set against `expected_watched_addresses`
/// and report the counts, or "unknown" if the comparison itself could not
/// run.
async fn probe_watch_reconciliation(
    evm_monitor: &RedisEVMMonitor,
    data_service: &data_service::PgDataService,
) -> WatchReconciliationHealth {
    match tokio::time::timeout(PROBE_TIMEOUT, reconcile_watches(data_service, evm_monitor)).await {
        Ok(Ok(counts)) => WatchReconciliationHealth {
            // The comparison running cleanly and finding a fault are two
            // different things - a stale or, worse, a missed watch is
            // exactly what this probe exists to surface, so `status` must
            // not read "ok" while either count is nonzero.
            status: if counts.stale == 0 && counts.missed == 0 {
                "ok"
            } else {
                "error"
            }
            .to_string(),
            stale_watches: counts.stale,
            missed_watches: counts.missed,
            error: None,
        },
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "deep health: failed to reconcile watch sets");
            WatchReconciliationHealth::unknown(e.to_string())
        }
        Err(_) => {
            tracing::warn!("deep health: watch reconciliation timed out");
            WatchReconciliationHealth::unknown("timeout")
        }
    }
}

/// Fetch the `SENTRY_RELEASE` evmmonitor was compiled with, as relayed
/// through its own health channel. `None` on timeout or fetch error, same as
/// an evmmonitor that never observed one.
async fn probe_evmmonitor_sentry_release(evm_monitor: &RedisEVMMonitor) -> Option<String> {
    match tokio::time::timeout(PROBE_TIMEOUT, evm_monitor.get_sentry_release()).await {
        Ok(Ok(release)) => release,
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "deep health: failed to get evmmonitor sentry release");
            None
        }
        Err(_) => {
            tracing::warn!("deep health: evmmonitor sentry release fetch timed out");
            None
        }
    }
}

/// Whether a chain-health snapshot represents fresh monitor data.
///
/// An empty snapshot means the health key never parsed - that is never
/// fresh, regardless of how the `Vec` got empty. A non-empty one is fresh
/// only if every chain in it is itself healthy: connected and not lagging
/// behind the block number it just reported. `is_healthy` already carries
/// that lag check, so this just refuses to call a stalled chain "fresh"
/// because *something* answered.
pub(super) fn chains_are_fresh(chains: &[ChainHealth]) -> bool {
    !chains.is_empty() && chains.iter().all(|chain| chain.is_healthy)
}

pub(super) fn build_rpc_map(
    chains: Vec<ChainHealth>,
    latency_ms: u64,
) -> HashMap<String, RpcHealth> {
    chains
        .into_iter()
        .map(|chain| {
            let status = if chain.is_healthy { "ok" } else { "error" };
            let error = if chain.is_healthy {
                None
            } else {
                Some(match &chain.status {
                    SourceStatus::Failed(msg) => msg.clone(),
                    SourceStatus::Disconnected => "disconnected".to_string(),
                    SourceStatus::Connecting => "connecting".to_string(),
                    // Connected but not is_healthy means one thing: the RPC
                    // answers fine and the processing loop isn't keeping up
                    // with it. Say the lag instead of a bare "unhealthy" -
                    // that's the difference between a two-minute read and an
                    // hour spent assuming the RPC itself was the problem.
                    SourceStatus::Connected => {
                        match (chain.current_block, chain.last_processed_block) {
                            (Some(current), Some(last)) => format!(
                                "connected but {} blocks behind",
                                current.saturating_sub(last)
                            ),
                            _ => "connected but has not processed a block yet".to_string(),
                        }
                    }
                })
            };
            (
                chain.chain_id.to_string(),
                RpcHealth {
                    status: status.to_string(),
                    latency_ms,
                    last_block: chain.current_block,
                    error,
                },
            )
        })
        .collect()
}
