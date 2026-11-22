//! Health check endpoints.

use std::time::Duration;

mod deep;
mod handlers;
mod models;
#[cfg(test)]
mod tests;

pub use deep::{__path_deep_health, deep_health};
pub use handlers::{
    __path_chains_health, __path_health_check, __path_liveness, __path_prometheus_metrics,
    __path_readiness, chains_health, health_check, liveness, prometheus_metrics,
    prometheus_metrics_unauthenticated, readiness,
};
pub use models::{
    ChainHealthInfo, ChainsHealthResponse, DeepHealthResponse, DependencyHealth, HealthResponse,
    MonitorHealth, ReadinessResponse, RpcHealth,
};

/// Timeout for each individual dependency check in readiness/deep probes.
const PROBE_TIMEOUT: Duration = Duration::from_secs(1);
