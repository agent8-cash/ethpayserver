//! RPC metrics recording helpers.
//!
//! Uses the `metrics` crate's global recorder to emit per-RPC-call counters
//! and histograms. `server` installs its own recorder at startup
//! (`server::metrics::init_metrics`); `evmmonitor` - the process that
//! actually makes these RPC calls - installs its own, see
//! [`monitor_recorder`] below.

use crate::error::EvmError;
use metrics::{counter, histogram};
use std::time::{Duration, Instant};

/// Record a successful RPC call.
pub fn record_rpc_ok(chain_id: u64, method: &str, duration: Duration) {
    let chain = chain_id.to_string();
    histogram!(
        "ethpayserver_rpc_request_duration_seconds",
        "chain_id" => chain.clone(),
        "method" => method.to_string()
    )
    .record(duration.as_secs_f64());

    counter!(
        "ethpayserver_rpc_requests_total",
        "chain_id" => chain,
        "method" => method.to_string(),
        "status" => "ok"
    )
    .increment(1);
}

/// Record a failed RPC call with error classification.
pub fn record_rpc_err(chain_id: u64, method: &str, duration: Duration, error: &EvmError) {
    let chain = chain_id.to_string();
    histogram!(
        "ethpayserver_rpc_request_duration_seconds",
        "chain_id" => chain.clone(),
        "method" => method.to_string()
    )
    .record(duration.as_secs_f64());

    counter!(
        "ethpayserver_rpc_requests_total",
        "chain_id" => chain.clone(),
        "method" => method.to_string(),
        "status" => "err"
    )
    .increment(1);

    let error_kind = classify_error(error);
    counter!(
        "ethpayserver_rpc_errors_total",
        "chain_id" => chain,
        "method" => method.to_string(),
        "error_kind" => error_kind
    )
    .increment(1);
}

/// Classify an EvmError into a metrics-friendly error kind.
fn classify_error(error: &EvmError) -> &'static str {
    let msg = error.to_string().to_lowercase();
    if msg.contains("timeout") || msg.contains("timed out") {
        "timeout"
    } else if msg.contains("rate limit") || msg.contains("429") || msg.contains("too many") {
        "rate_limited"
    } else if msg.contains("connection")
        || msg.contains("dns")
        || msg.contains("network")
        || msg.contains("reset")
        || msg.contains("refused")
        || msg.contains("broken pipe")
    {
        "network"
    } else {
        "other"
    }
}

/// Helper to time an RPC call and record metrics on both success and failure.
pub async fn timed_rpc<F, T>(chain_id: u64, method: &str, fut: F) -> Result<T, EvmError>
where
    F: std::future::Future<Output = Result<T, EvmError>>,
{
    let start = Instant::now();
    let result = fut.await;
    let duration = start.elapsed();
    match &result {
        Ok(_) => record_rpc_ok(chain_id, method, duration),
        Err(e) => record_rpc_err(chain_id, method, duration, e),
    }
    result
}

/// Installs a Prometheus recorder and serves it, for `evmmonitor` only.
///
/// `evmmonitor` is the process that actually makes the chain RPC calls
/// `record_rpc_ok`/`record_rpc_err` above count, but - unlike `server`,
/// which installs its own recorder in `server::metrics::init_metrics` - it
/// never installed any global `metrics` recorder, so every one of those
/// counters was a silent no-op in production. No Sentry fanout here: that
/// alerting path is `server`'s own concern, not needed just to make these
/// counters observable for the RPC-volume benchmark.
#[cfg(feature = "monitor-bin")]
pub mod monitor_recorder {
    use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
    use std::sync::OnceLock;

    static METRICS_HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();

    /// Installs the global recorder. Call once, before any RPC call.
    pub fn init() -> anyhow::Result<()> {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::set_global_recorder(recorder)
            .map_err(|_| anyhow::anyhow!("metrics recorder already installed"))?;
        METRICS_HANDLE
            .set(handle)
            .map_err(|_| anyhow::anyhow!("monitor_recorder::init called twice"))?;
        Ok(())
    }

    pub(crate) fn render() -> Option<String> {
        METRICS_HANDLE.get().map(PrometheusHandle::render)
    }

    /// Loopback-only router: unauthenticated `/metrics`, nothing else -
    /// mirrors `server::api::metrics_router`.
    pub fn router() -> axum::Router {
        axum::Router::new().route(
            "/metrics",
            axum::routing::get(|| async {
                match render() {
                    Some(body) => (
                        axum::http::StatusCode::OK,
                        [("content-type", "text/plain; version=0.0.4; charset=utf-8")],
                        body,
                    ),
                    None => (
                        axum::http::StatusCode::SERVICE_UNAVAILABLE,
                        [("content-type", "text/plain")],
                        "Metrics not initialized".to_string(),
                    ),
                }
            }),
        )
    }
}

#[cfg(all(test, feature = "monitor-bin"))]
mod monitor_recorder_tests {
    use super::monitor_recorder;
    use crate::error::EvmError;
    use std::time::Duration;

    /// Proves the gap this module closes: before `init()`, a recorded RPC
    /// call really did go nowhere (no global recorder => no renderable
    /// handle); after it, the same call site's counter shows up in
    /// `/metrics` output.
    #[test]
    fn init_makes_rpc_counters_observable() {
        super::record_rpc_ok(11155111, "eth_getLogs", Duration::from_millis(5));
        assert!(
            monitor_recorder::render().is_none(),
            "no recorder installed yet - this call must still be a no-op"
        );

        monitor_recorder::init().expect("first init in this process must succeed");

        super::record_rpc_ok(11155111, "eth_getLogs", Duration::from_millis(5));
        let rendered = monitor_recorder::render().expect("handle set by init()");
        assert!(
            rendered.contains("ethpayserver_rpc_requests_total"),
            "rendered metrics missing the RPC counter: {rendered}"
        );
        assert!(rendered.contains("chain_id=\"11155111\""));
        assert!(rendered.contains("method=\"eth_getLogs\""));

        let err = EvmError::Rpc("boom".into());
        super::record_rpc_err(1, "eth_call", Duration::from_millis(1), &err);
        let rendered = monitor_recorder::render().expect("handle still set");
        assert!(rendered.contains("ethpayserver_rpc_errors_total"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::EvmError;

    #[test]
    fn test_classify_timeout() {
        let err = EvmError::Timeout("request timed out".into());
        assert_eq!(classify_error(&err), "timeout");
    }

    #[test]
    fn test_classify_rate_limited() {
        let err = EvmError::Rpc("429 Too Many Requests".into());
        assert_eq!(classify_error(&err), "rate_limited");
    }

    #[test]
    fn test_classify_network() {
        let err = EvmError::Connection("connection refused".into());
        assert_eq!(classify_error(&err), "network");
    }

    #[test]
    fn test_classify_other() {
        let err = EvmError::Rpc("invalid response".into());
        assert_eq!(classify_error(&err), "other");
    }
}
